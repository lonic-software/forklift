use serde::Serialize;
use crate::commands::shift;
use crate::output::{self, CommandOutput};
use forklift_core::util::{file_utils, merge_utils};
use forklift_core::util::pallet_utils::{self, PalletNamespace, PalletRef};
use forklift_core::util::remote_utils::RemoteClient;

/// Handle the palletize command.
/// * `palletize <name>`            - Create a new pallet pointing at the current pallet's
///                                   head and shift to it (like git's "checkout -b").
/// * `palletize <name> <revision>` - Create the new pallet at the given revision (a
///                                   pallet name or a parcel hash / unique hash prefix)
///                                   and shift to it.
/// * `palletize`                   - List the pallets, marking the current one.
/// * `palletize --remote`          - List the remote's pallets, each against the local one.
///
/// # Arguments
/// * `name`     - The name of the new pallet (`None` lists the pallets).
/// * `revision` - Where the new pallet starts (`None` means the current pallet's head).
/// * `all`      - When listing, also show the meta pallets (the office, etc.).
/// * `remote`   - List the remote's pallets instead of the local ones.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(name: Option<String>, revision: Option<String>, all: bool, remote: bool) -> Result<(), String> {
    match name {
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

            pallet_utils::update_pallet_head(name, None, &parcel_hash, &pallet_utils::RefPreconditions::NONE)?;

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
        crate::docgen::OutputSchema::of::<PalletList>(),
        crate::docgen::OutputSchema::of::<RemotePalletList>(),
    ]
}
