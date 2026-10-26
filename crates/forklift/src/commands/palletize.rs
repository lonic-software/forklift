use serde::Serialize;
use crate::commands::shift;
use crate::output::{self, CommandOutput};
use forklift_core::error;
use forklift_core::util::{bay_utils, file_utils, haul_utils, journal_utils, merge_utils, take_utils};
use forklift_core::util::take_utils::{FetchAt, TakeTrust};
use forklift_core::util::haul_utils::HaulStatus;
use forklift_core::util::pallet_utils::{self, PalletNamespace, PalletRef, PalletsPrecondition, RefPreconditions, RefUpdateError};
use forklift_core::util::remote_utils::{self, RemoteClient};

/// Handle the palletize command.
/// * `palletize <name>`            - Create a new pallet pointing at the current pallet's
///                                   head and shift to it (like git's "checkout -b").
/// * `palletize <name> <revision>` - Create the new pallet at the given revision (a
///                                   pallet name or a parcel hash / unique hash prefix)
///                                   and shift to it.
/// * `palletize`                   - List the pallets, marking the current one.
/// * `palletize --remote`          - List the remote's pallets, each against the local one.
/// * `palletize --delete <name>`   - Delete the pallet (`--force`: even when nothing else holds
///                                   its work).
///
/// # Arguments
/// * `name`     - The name of the new pallet (`None` lists the pallets).
/// * `revision` - Where the new pallet starts (`None` means the current pallet's head).
/// * `all`      - When listing, also show the meta pallets (the office, etc.).
/// * `remote`   - List the remote's pallets instead of the local ones.
/// * `delete`   - Delete pallet `name` instead of creating it.
/// * `force`    - With `delete`: delete even when no other pallet holds its work.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(name: Option<String>,
                            revision: Option<String>,
                            all: bool,
                            remote: bool,
                            delete: bool,
                            force: bool) -> Result<(), String> {
    match name {
        Some(name) if delete => delete_pallet(&name, force).await,
        Some(name) => create_pallet(&name, revision.as_deref()).await,
        None if remote => list_remote_pallets(all).await,
        None => list_pallets(all),
    }
}

/// Create a new pallet and make it the current pallet.
///
/// # Arguments
/// * `name`     - The name of the new pallet.
/// * `revision` - Where the new pallet starts: a pallet name or a parcel hash (prefix).
///                `None` means the current pallet's head.
///
/// # Returns
/// * `Ok(())`      - If the pallet was created.
/// * `Err(String)` - If the name is invalid, the pallet already exists, or the revision
///                   could not be resolved (or materialized).
async fn create_pallet(name: &str, revision: Option<&str>) -> Result<(), String> {
    pallet_utils::validate_pallet_name(name)?;

    let position = pallet_utils::read_position()?;
    let current = position.pallet.clone();

    if name == current {
        return Err(format!("\"{}\" is already the current pallet.", name));
    }

    if pallet_utils::does_pallet_exist(name) {
        return Err(format!(
            "Pallet \"{}\" already exists. Use the \"shift\" command to move to it.",
            name
        ));
    }

    // Before the ref is created, so a refusal leaves no half-made pallet behind.
    shift::refuse_mid_merge("switching to a new pallet")?;

    match revision {
        // The new pallet starts at the given revision, which may differ from the current
        // head — a full shift materializes it (and refuses on a dirty warehouse).
        Some(revision) => {
            let parcel_hash = pallet_utils::resolve_revision(revision)?;

            // A hash can name history a lower fetched and refused, so it is verified like any
            // take: every local pallet head must be one this warehouse's anchor vouches for. A
            // pallet name resolves to a head that already is.
            let preconditions = take_utils::verify_take(None, &TakeTrust::local()?, &parcel_hash, name, FetchAt::Warehouse).await?;

            pallet_utils::update_pallet_head(name, None, &parcel_hash, &preconditions)?;

            let head = shift::shift_to(name).await?;

            output::emit(&Palletized {
                pallet: name.to_string(),
                head: Some(head),
                at_revision: Some(parcel_hash),
            });

            Ok(())
        }

        // The new pallet starts at the parcel this bay's files stand on (the current pallet's
        // head, unless another bay moved it since) — same tree, so only the refs move. If the
        // bay stands on nothing (an unborn pallet), the new pallet is unborn as well: it will be
        // born by the first parcel stacked on it.
        None => {
            let head = position.base.clone();

            if let Some(head) = &head {
                pallet_utils::update_pallet_head(name, None, head, &pallet_utils::RefPreconditions::NONE)?;
            }

            pallet_utils::write_position(&pallet_utils::BayPosition::new(name, head.clone()))?;

            output::emit(&Palletized {
                pallet: name.to_string(),
                head,
                at_revision: None,
            });

            Ok(())
        }
    }
}

/// A newly created pallet.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Palletized {
    pallet: String,

    /// The head the new pallet points at (`null` when created unborn from an unborn
    /// current pallet).
    head: Option<String>,

    /// The revision the pallet was created at, when one was given (otherwise it shares
    /// the current pallet's head).
    #[serde(skip_serializing_if = "Option::is_none")]
    at_revision: Option<String>,
}

impl CommandOutput for Palletized {
    fn render_human(&self) {
        match &self.at_revision {
            // Reproduces the two-line report: "Created … at parcel …" then the shift line.
            Some(parcel) => {
                println!("Created pallet \"{}\" at parcel {}.", self.pallet, parcel);

                if let Some(head) = &self.head {
                    println!("Shifted to pallet \"{}\" (head {}).", self.pallet, head);
                }
            }
            None => println!("Created pallet \"{}\" and shifted to it.", self.pallet),
        }
    }
}

/// Delete user pallet `name`: remove its ref and keep its parcels. The removal writes the delete's
/// undo entry in its own ref-lock hold, so the deleted head stays rooted (gc, repack and
/// scope-prune keep it) and `undo` can bring the pallet back, until the entry is undone, pushed out
/// of this bay's journal, or the bay is removed.
///
/// Refused for a meta pallet; for a pallet any checkout stands on; for the source or target of an
/// open haul (or when a haul cannot be read); and, unless `force`, for a pallet whose head no other
/// local user pallet reaches (that pallet is pinned at the head it was read at, so two bays cannot
/// each delete the other's only witness) and no head of the configured remote reaches, as walked
/// over the history held here.
///
/// # Returns
/// * `Ok(())`      - If the pallet was deleted.
/// * `Err(String)` - The refusal, or why the delete failed (nothing deleted, unless it says so).
async fn delete_pallet(name: &str, force: bool) -> Result<(), String> {
    let reference = PalletRef::parse(name)?;

    if reference.namespace == PalletNamespace::Meta {
        return Err(format!(
            "\"{}\" is a meta pallet: meta pallets hold the office, hauls and tags, and are never \
            deleted.",
            name
        ));
    }

    let Some(head) = pallet_utils::get_pallet_head(&reference.name)? else {
        return Err(pallet_utils::pallet_not_found_refusal(&reference).into());
    };

    if let Some((which, _)) = bay_utils::bays_on(&reference.name)?.into_iter().next() {
        let mut which = which;
        which[..1].make_ascii_uppercase();

        return Err(format!(
            "{} stands on pallet \"{}\"; shift it to another pallet before deleting this one.",
            which, reference.name
        ));
    }

    let merged_hauls = refuse_an_open_haul(&reference.name, &haul_utils::read_hauls()?)?;

    let (contained_by, preconditions) = if force {
        (None, RefPreconditions::NONE)
    } else {
        containment(&reference.name, &head).await?
    };

    #[cfg(test)]
    tests::AFTER_CONTAINMENT.with(|seam| seam.take().map(|seam| seam()));

    let entry = journal_utils::delete_entry(&reference.name, &head)?;

    match pallet_utils::remove_pallet_head_in(PalletNamespace::User, &reference.name, &head, &preconditions, &entry) {
        Ok(()) => {}
        Err(RefUpdateError::Moved(_)) => return Err(format!(
            "Pallet \"{}\" moved while it was being deleted; nothing was deleted. Run the command \
            again.",
            reference.name
        )),
        // A failed removal can still have landed: what the ref holds now decides.
        Err(RefUpdateError::Failed(e)) if pallet_utils::get_pallet_head(&reference.name) != Ok(None) => return Err(e),
        Err(RefUpdateError::Failed(e)) => return Err(format!(
            "{} The pallet's ref is gone all the same, so it was deleted; \"forklift undo\" brings \
            it back.",
            e
        )),
        Err(e) => return Err(e.to_string()),
    }

    output::emit(&Deleted {
        pallet: reference.name,
        head,
        contained_by,
        merged_hauls,
    });

    Ok(())
}

/// Refuse a delete of `pallet` while an open haul in `hauls` names it as its source or target, or
/// while a haul cannot be read (it might). Returns the merged hauls whose target it is: their proof will
/// read as unproven once the target is gone.
fn refuse_an_open_haul(pallet: &str, hauls: &haul_utils::HaulsRead) -> Result<Vec<String>, String> {
    if let Some(unreadable) = hauls.unreadable.first() {
        return Err(format!(
            "Cannot tell whether haul {} names pallet \"{}\": {} Nothing was deleted.",
            unreadable.id, pallet, unreadable.reason
        ));
    }

    let mut merged = Vec::new();

    for haul in &hauls.hauls {
        let names = haul.opened.source == pallet || haul.opened.target == pallet;

        match haul.status {
            HaulStatus::Open if names => return Err(format!(
                "Open haul {} (\"{}\" into \"{}\") names pallet \"{}\"; merge or close the haul \
                before deleting the pallet.",
                haul.id, haul.opened.source, haul.opened.target, pallet
            )),
            HaulStatus::Merged(_) if haul.opened.target == pallet => merged.push(haul.id.clone()),
            _ => {}
        }
    }

    Ok(merged)
}

/// What holds `head` besides `pallet`: another local user pallet reaching it, which the removal
/// pins as its witness, else a head of the configured remote reaching it over the history held
/// here (no local write can remove a remote head, so that needs no precondition). Neither is a
/// refusal naming `--force`.
async fn containment(pallet: &str, head: &str) -> Result<(Option<String>, RefPreconditions), String> {
    // A history that cannot be read is never taken as "does not hold it", which would steer the
    // user to "--force": the first such error is the refusal when nothing else holds the work.
    let mut unreadable: Option<String> = None;
    let reaches = |which: &str, other: &str| -> Result<bool, String> {
        if other == head {
            return Ok(true);
        }

        merge_utils::is_ancestor(head, other).map_err(|e| format!(
            "Cannot tell whether {} holds the work on pallet \"{}\": {} Nothing was deleted.",
            which, pallet, e
        ))
    };

    for (reference, other) in pallet_utils::all_pallet_refs()? {
        if reference.namespace != PalletNamespace::User || reference.name == pallet {
            continue;
        }

        let held = match reaches(&format!("pallet \"{}\"", reference.name), &other) {
            Ok(held) => held,
            Err(e) => {
                unreadable.get_or_insert(e);
                false
            }
        };

        if held {
            let preconditions = RefPreconditions {
                pallets: PalletsPrecondition::Witness { pallet: reference.name.clone(), head: other },
                ..RefPreconditions::NONE
            };

            return Ok((Some(format!("pallet \"{}\"", reference.name)), preconditions));
        }
    }

    let unheld = format!(
        "No other pallet here holds the work on pallet \"{}\" ({}), so deleting it could lose \
        that work. Merge it somewhere first, or rerun with \"--force\": the work then stays \
        recoverable by \"forklift undo\" only while the delete is in this bay's undo journal (until \
        it is undone, pushed out by {} later journaled operations here, or this bay is removed), and \
        is collectable after that.",
        pallet, head, journal_utils::MAX_ENTRIES
    );

    if !RemoteClient::is_configured() {
        return Err(unreadable.unwrap_or(unheld));
    }

    let note = "The delete asks the remote whether it holds the pallet's work; nothing was deleted.";
    let client = RemoteClient::from_config().map_err(|e| error::append_to_message(e, note))?;
    let info = client.fetch_info().await.map_err(|e| error::append_to_message(e, note))?;
    remote_utils::check_remote_trust(&client, &info, None).map_err(|e| error::append_to_message(e, note))?;

    let mut held = false;
    for (wire, other) in &info.pallets {
        let reached = file_utils::does_object_exist(other)
            .and_then(|present| Ok(present && reaches(&format!("the remote's pallet \"{}\"", wire), other)?));

        match reached {
            Ok(true) => {
                held = true;
                break;
            }
            Ok(false) => {}
            Err(e) => {
                unreadable.get_or_insert(e);
            }
        }
    }

    if held {
        return Ok((Some("the remote".to_string()), RefPreconditions::NONE));
    }

    Err(unreadable.unwrap_or(format!("{} Neither does the remote.", unheld)))
}

/// A deleted pallet.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Deleted {
    /// The pallet that was deleted.
    pallet: String,

    /// The head it was deleted at; its parcels stay, and `undo` brings the pallet back here.
    head: String,

    /// What else holds that head (`pallet "<name>"` or `the remote`); `null` when the delete was
    /// forced without asking.
    contained_by: Option<String>,

    /// Merged hauls whose target this was: their proof now reads as unproven, since no pallet
    /// holds the target any more. Never a claim the record is false.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    merged_hauls: Vec<String>,
}

impl CommandOutput for Deleted {
    fn render_human(&self) {
        println!("Deleted pallet \"{}\" (it was at {}).", self.pallet, self.head);

        match &self.contained_by {
            Some(by) => println!("Its work is still held by {}.", by),
            None => println!("Nothing else was checked for its work: it stays recoverable only while this delete can be undone."),
        }

        for haul in &self.merged_hauls {
            println!("Haul {} was merged into it; its proof now reads as unproven, as no pallet holds the target.", haul);
        }

        println!("\"forklift undo\" brings it back.");
    }
}

/// List the pallets, marking the current one with a `*`. Only user pallets are shown by
/// default; `all` adds the meta pallets (the office, etc.) under a separate heading,
/// each with its `@` qualifier so it reads as the address you use to reach it.
/// The current pallet is included even when it is unborn (it has no ref file yet).
///
/// # Arguments
/// * `all` - Whether to also list the meta pallets.
///
/// # Returns
/// * `Ok(())`      - If the pallets were listed.
/// * `Err(String)` - If a pallets folder could not be read, or a pallet's ref could not be read.
fn list_pallets(all: bool) -> Result<(), String> {
    let current = pallet_utils::read_position()?.pallet;
    let mut names = pallet_utils::list_pallets()?;

    // The current pallet is listed even when unborn (it has no ref file yet) — folded
    // into `pallets` itself (with `head: null`) rather than left as a side flag only, so
    // a JSON consumer building a pallet graph never needs a special case for "the one
    // pallet that isn't in the list".
    let current_unborn = !names.contains(&current);
    if current_unborn {
        names.push(current.clone());
        names.sort();
    }

    let pallets = names.into_iter()
        .map(|name| {
            let head = pallet_utils::get_pallet_head(&name)?;
            Ok(PalletEntry { current: name == current, name, head })
        })
        .collect::<Result<Vec<_>, String>>()?;

    // Meta pallets are addressed by their qualified form (`@office`), only when asked.
    let meta = if all {
        pallet_utils::list_meta_pallets()?
            .into_iter()
            .map(|name| PalletRef::meta(name).to_wire())
            .collect()
    } else {
        Vec::new()
    };

    output::emit(&PalletList { current, current_unborn, pallets, meta });

    Ok(())
}

/// The list of pallets, marking the current one.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PalletList {
    /// The current pallet (HEAD equivalent).
    current: String,

    /// Whether the current pallet is unborn (no parcel stacked on it yet).
    current_unborn: bool,

    /// Every user pallet with a ref file.
    pallets: Vec<PalletEntry>,

    /// The meta pallets in their qualified form (`@office`), present only when `--all`
    /// was given.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta: Vec<String>,
}

/// One pallet in the list.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PalletEntry {
    name: String,
    current: bool,

    /// The pallet's head parcel hash; `null` when it is unborn (nothing stacked on it yet).
    head: Option<String>,
}

impl CommandOutput for PalletList {
    fn render_human(&self) {
        if self.current_unborn {
            println!("* {} (unborn)", self.current);
        }

        for entry in &self.pallets {
            // The unborn current pallet was already printed above (with its "(unborn)"
            // marker) — skip it here so it is not shown twice.
            if entry.current && self.current_unborn {
                continue;
            }

            let marker = if entry.current { "*" } else { " " };
            println!("{} {}", marker, entry.name);
        }

        if !self.meta.is_empty() {
            println!("Meta pallets:");

            for name in &self.meta {
                println!("  {}", name);
            }
        }
    }
}

/// List the remote's pallets from its handshake, each with how it relates to the local pallet of
/// that name. Only user pallets are shown by default; `all` adds the meta pallets. Nothing is
/// downloaded beyond the handshake, and nothing is written: the trust sync and fetch belong to
/// `lower`, which this listing points at.
///
/// # Arguments
/// * `all` - Whether to also list the meta pallets.
///
/// # Returns
/// * `Ok(())`      - If the pallets were listed.
/// * `Err(String)` - If no remote is configured, the handshake failed, or a local ref or parcel
///                   could not be read.
async fn list_remote_pallets(all: bool) -> Result<(), String> {
    let client = RemoteClient::from_config()?;
    let info = client.fetch_info().await?;

    let mut pallets = Vec::new();
    let mut meta = Vec::new();
    let mut unnamed = Vec::new();

    for (wire, remote_head) in &info.pallets {
        // A remote with other naming rules may list names this build rejects: they are reported
        // apart, uncompared, rather than failing the listing of every other pallet.
        let Ok(reference) = PalletRef::parse(wire) else {
            unnamed.push(wire.clone());
            continue;
        };

        let entries = match reference.namespace {
            PalletNamespace::User => &mut pallets,
            PalletNamespace::Meta if all => &mut meta,
            PalletNamespace::Meta => continue,
        };

        let local_head = pallet_utils::get_pallet_head_in(reference.namespace, &reference.name)?;
        let relation = relation_to_remote(remote_head, local_head.as_deref())?;

        entries.push(RemotePalletEntry { name: reference.to_wire(), remote_head: remote_head.clone(), local_head, relation });
    }

    output::emit(&RemotePalletList { remote: client.url().to_string(), pallets, meta, unnamed });

    Ok(())
}

/// How the local pallet at `local_head` relates to the remote's at `remote_head`. A remote head
/// whose parcel is not here cannot be a local ancestor — a parcel is fetched whole, and a ref
/// moves only once its closure is present — so it differs, without a walk; behind and diverged
/// cannot be told apart without fetching, which `lower` does.
fn relation_to_remote(remote_head: &str, local_head: Option<&str>) -> Result<RemoteRelation, String> {
    let Some(local_head) = local_head else {
        return Ok(RemoteRelation::Absent);
    };

    if local_head == remote_head {
        return Ok(RemoteRelation::Same);
    }

    if file_utils::does_object_exist(remote_head)? && merge_utils::is_ancestor(remote_head, local_head)? {
        return Ok(RemoteRelation::LocalAhead);
    }

    Ok(RemoteRelation::Differs)
}

/// The remote's pallets, each against the local pallet of that name.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemotePalletList {
    /// The remote's URL.
    remote: String,

    /// Every user pallet the remote has with something stacked.
    pallets: Vec<RemotePalletEntry>,

    /// The remote's meta pallets in their qualified form (`@office`), listed only when `--all`
    /// was given; omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta: Vec<RemotePalletEntry>,

    /// Pallet names the remote lists that this build cannot read (another version's naming
    /// rules), so they are not compared; omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unnamed: Vec<String>,
}

/// One remote pallet.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemotePalletEntry {
    name: String,

    /// The remote's head parcel hash.
    remote_head: String,

    /// The local pallet's head parcel hash; `null` when there is no local pallet of that name.
    local_head: Option<String>,

    relation: RemoteRelation,
}

/// How a local pallet relates to the remote's pallet of the same name.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteRelation {
    /// There is no local pallet of that name; `lower <name>` creates it.
    Absent,

    /// The local pallet is at the remote head.
    Same,

    /// The local pallet holds the remote head and more; `lift` publishes it.
    LocalAhead,

    /// The local pallet is behind the remote or diverged from it; `lower <name>` tells which,
    /// fast-forwarding a pallet that is behind.
    Differs,
}

impl CommandOutput for RemotePalletList {
    fn render_human(&self) {
        println!("Pallets on {}:", forklift_core::error::sanitize(&self.remote));

        if self.pallets.is_empty() {
            println!("  (none)");
        }

        for entry in &self.pallets {
            println!("  {}", describe_remote_entry(entry, PalletNamespace::User));
        }

        if !self.meta.is_empty() {
            println!("Meta pallets:");

            for entry in &self.meta {
                println!("  {}", describe_remote_entry(entry, PalletNamespace::Meta));
            }
        }

        if !self.unnamed.is_empty() {
            println!("Not compared (names this version of forklift cannot read):");

            for name in &self.unnamed {
                println!("  {}", forklift_core::error::sanitize(name));
            }
        }
    }
}

fn describe_remote_entry(entry: &RemotePalletEntry, namespace: PalletNamespace) -> String {
    // A meta pallet is synced by every lower, never lowered by name.
    if namespace == PalletNamespace::Meta {
        return match entry.relation {
            RemoteRelation::Same => format!("{} (same as here)", entry.name),
            RemoteRelation::LocalAhead => format!("{} (ahead here)", entry.name),
            RemoteRelation::Absent | RemoteRelation::Differs => format!("{} (\"forklift lower\" syncs it)", entry.name),
        };
    }

    match entry.relation {
        RemoteRelation::Absent => format!("{} (not here; \"forklift lower {}\" brings it in)", entry.name, entry.name),
        RemoteRelation::Same => format!("{} (same as here)", entry.name),
        RemoteRelation::LocalAhead => format!("{} (ahead here; \"lift\" from it publishes it)", entry.name),
        RemoteRelation::Differs => format!("{} (differs; \"forklift lower {}\" fast-forwards or reports a divergence)", entry.name, entry.name),
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Palletized>(),
        crate::docgen::OutputSchema::of::<Deleted>(),
        crate::docgen::OutputSchema::of::<PalletList>(),
        crate::docgen::OutputSchema::of::<RemotePalletList>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Warehouse};

    thread_local! {
        /// Runs once just after a delete judged its containment, before the removal.
        pub(super) static AFTER_CONTAINMENT: std::cell::Cell<Option<Box<dyn FnOnce()>>> = std::cell::Cell::new(None);
    }

    #[test]
    fn two_deletes_of_each_others_witness_cannot_both_land() {
        let _warehouse = Warehouse::storage_only("delete-witnesses");
        let none = RefPreconditions::NONE;

        // "x" and "y" share a head that "main" does not reach: each is the other's only witness.
        let shared = test_support::store_parcel(&test_support::store_tree("a.txt", b"a\n"), &[]);
        let unrelated = test_support::store_parcel(&test_support::store_tree("b.txt", b"b\n"), &[]);
        pallet_utils::update_pallet_head("main", None, &unrelated, &none).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(unrelated))).unwrap();
        pallet_utils::update_pallet_head("x", None, &shared, &none).unwrap();
        pallet_utils::update_pallet_head("y", None, &shared, &none).unwrap();

        // Another bay deletes "y", witnessed by "x", after this delete of "x" judged "y" its witness.
        let head = shared.clone();
        AFTER_CONTAINMENT.with(|seam| seam.set(Some(Box::new(move || {
            let entry = journal_utils::delete_entry("y", &head).unwrap();
            let witness = RefPreconditions {
                pallets: PalletsPrecondition::Witness { pallet: "x".to_string(), head: head.clone() },
                ..RefPreconditions::NONE
            };
            pallet_utils::remove_pallet_head_in(PalletNamespace::User, "y", &head, &witness, &entry).unwrap();
        }))));

        let refused = test_support::block_on(delete_pallet("x", false)).expect_err("the second delete must refuse");
        assert!(refused.contains("Run it again"), "{}", refused);
        assert_eq!(pallet_utils::get_pallet_head("x").unwrap(), Some(shared), "x stays");
        assert_eq!(pallet_utils::get_pallet_head("y").unwrap(), None);
        assert_eq!(journal_utils::removed_heads().unwrap().len(), 1, "only the landed delete is journaled");
    }

    #[test]
    fn a_delete_reports_a_witness_history_it_cannot_read() {
        let _warehouse = Warehouse::storage_only("delete-unreadable-witness");
        let none = RefPreconditions::NONE;

        let main = test_support::store_parcel(&test_support::store_tree("a.txt", b"a\n"), &[]);
        pallet_utils::update_pallet_head("main", None, &main, &none).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(main))).unwrap();
        let side = test_support::store_parcel(&test_support::store_tree("s.txt", b"s\n"), &[]);
        pallet_utils::update_pallet_head("side", None, &side, &none).unwrap();

        // "x" stands on a parcel whose parent this store does not hold.
        let missing = "9".repeat(64);
        let x = test_support::store_parcel(&test_support::store_tree("x.txt", b"x\n"), &[missing.as_str()]);
        test_support::write_ref_unchecked("x", &x);

        let refused = test_support::block_on(delete_pallet("side", false)).unwrap_err();
        assert!(refused.contains("Cannot tell whether pallet \"x\" holds the work"), "{}", refused);
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(side.clone()));

        // A witness listed after the unreadable one still holds the work.
        pallet_utils::update_pallet_head("y", None, &side, &none).unwrap();
        test_support::block_on(delete_pallet("side", false)).unwrap();
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);
    }

    #[test]
    fn a_delete_refuses_while_any_haul_cannot_be_read() {
        let hauls = haul_utils::HaulsRead {
            unreadable: vec![haul_utils::UnreadableHaul { id: "0123abcd".to_string(), reason: "its opened cell is damaged.".to_string() }],
            ..Default::default()
        };

        let refused = refuse_an_open_haul("side", &hauls).unwrap_err();
        assert!(refused.contains("Cannot tell whether haul 0123abcd names pallet \"side\""), "{}", refused);
        assert_eq!(refuse_an_open_haul("side", &haul_utils::HaulsRead::default()), Ok(Vec::new()));
    }
}
