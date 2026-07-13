use serde::Serialize;
use crate::commands::shift;
use crate::output::{self, CommandOutput};
use forklift_core::util::pallet_utils::{self, PalletRef};

/// Handle the palletize command.
/// * `palletize <name>`            - Create a new pallet pointing at the current pallet's
///                                   head and shift to it (like git's "checkout -b").
/// * `palletize <name> <revision>` - Create the new pallet at the given revision (a
///                                   pallet name or a parcel hash / unique hash prefix)
///                                   and shift to it.
/// * `palletize`                   - List the pallets, marking the current one.
///
/// # Arguments
/// * `name`     - The name of the new pallet (`None` lists the pallets).
/// * `revision` - Where the new pallet starts (`None` means the current pallet's head).
/// * `all`      - When listing, also show the meta pallets (the office, etc.).
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(name: Option<String>, revision: Option<String>, all: bool) -> Result<(), String> {
    match name {
        Some(name) => create_pallet(&name, revision.as_deref()).await,
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

    let current = pallet_utils::get_current_pallet_name()?;

    if name == current {
        return Err(format!("\"{}\" is already the current pallet.", name));
    }

    if pallet_utils::does_pallet_exist(name) {
        return Err(format!(
            "Pallet \"{}\" already exists. Use the \"shift\" command to move to it.",
            name
        ));
    }

    match revision {
        // The new pallet starts at the given revision, which may differ from the current
        // head — a full shift materializes it (and refuses on a dirty warehouse).
        Some(revision) => {
            let parcel_hash = pallet_utils::resolve_revision(revision)?;

            pallet_utils::set_pallet_head(name, &parcel_hash)?;

            let head = shift::shift_to(name).await?;

            output::emit("palletize", &Palletized {
                pallet: name.to_string(),
                head: Some(head),
                at_revision: Some(parcel_hash),
            });

            Ok(())
        }

        // The new pallet starts at the same head as the current one — same tree, so only
        // the refs move. If the current pallet is unborn (nothing stacked yet), the new
        // pallet is unborn as well: it will be born by the first parcel stacked on it.
        None => {
            let head = pallet_utils::get_pallet_head(&current)?;

            if let Some(head) = &head {
                pallet_utils::set_pallet_head(name, head)?;
            }

            pallet_utils::set_current_pallet_name(name)?;

            output::emit("palletize", &Palletized {
                pallet: name.to_string(),
                head,
                at_revision: None,
            });

            Ok(())
        }
    }
}

/// A newly created pallet.
#[derive(Serialize)]
struct Palletized {
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
    let current = pallet_utils::get_current_pallet_name()?;
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

    output::emit("palletize", &PalletList { current, current_unborn, pallets, meta });

    Ok(())
}

/// The list of pallets, marking the current one.
#[derive(Serialize)]
struct PalletList {
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
#[derive(Serialize)]
struct PalletEntry {
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
