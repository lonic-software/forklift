//! `heal`'s repair of the bay's position (which pallet the bay stands on, and the parcel its files
//! stand on), when it is missing or damaged.
//!
//! Only exact evidence rebuilds it. The position's copy, written right after it by every position
//! write ([`pallet_utils::write_position_in`]), is the last position the bay was given. Failing
//! that, a standing operation marker's source is where the bay stood when that operation began,
//! and the recovery that runs later in the same heal moves it on from there. The undo journal is
//! never used: most position writes are not journaled, so its newest entry can name a pallet the
//! bay has left while that pallet's head still equals the base it records. Without exact evidence
//! the operator chooses (`heal --position <pallet>`), taking the files as standing on that
//! pallet's head.
//!
//! A healthy position whose copy does not match it has its copy rewritten. A position a newer
//! version wrote is left alone.

use serde::Serialize;

use crate::error::{CoreError, RefusalCode};
use crate::globals::bay_root;
use crate::util::pallet_utils::{self, BayPosition, PositionUnread};
use crate::util::{journal_utils, operation_utils};

/// Where a rebuilt position came from.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSource {
    /// The position's own copy, the last position written.
    Copy,

    /// The source of the operation left unfinished, which recovery then moves on.
    Operation,

    /// The pallet `--position` named, at its head.
    Chosen,
}

/// A position `heal` wrote.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PositionRebuilt {
    /// The pallet the bay now stands on.
    pub pallet: String,

    /// The parcel its files are taken to stand on; absent for an unborn pallet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,

    /// What it was rebuilt from.
    pub from: PositionSource,
}

impl PositionRebuilt {
    /// The sentence a person is told.
    pub fn sentence(&self) -> String {
        let at = match &self.base {
            Some(base) => format!("parcel {}", base),
            None => "no parcel yet".to_string(),
        };
        let from = match self.from {
            PositionSource::Copy => "from its copy",
            PositionSource::Operation => "from the operation left unfinished, which recovery then took on",
            PositionSource::Chosen => "as --position chose; the files are taken as standing on that head, and \"forklift stocktake\" shows how they differ from it",
        };
        format!("Rebuilt the bay's position on pallet \"{}\" at {}, {}.", self.pallet, at, from)
    }
}

/// Repair the bay's position. `chosen` is the pallet `--position` named, if any.
///
/// # Returns
/// * `Ok(None)`                  - The position reads (its copy rewritten to match, if it did not),
///                                 or a newer version wrote it.
/// * `Ok(Some(PositionRebuilt))` - What was written.
/// * `Err(String)`               - A `bay_position_damaged` refusal when there is no exact evidence
///                                 and nothing was chosen; a refusal of `--position` on a position
///                                 that reads, or of a pallet it cannot stand on; or a failed
///                                 read or write. Nothing was changed but what a write says.
pub fn repair(chosen: Option<&str>) -> Result<Option<PositionRebuilt>, String> {
    let dir = bay_root();
    let path = dir.join(pallet_utils::FILE_NAME_POSITION);

    let unread = match std::fs::read(&path) {
        Ok(bytes) => match pallet_utils::decode_position(&bytes) {
            Ok(position) => return keep(&dir, position, chosen),
            Err(PositionUnread::Newer(reason)) => return match chosen {
                Some(_) => Err(format!(
                    "The bay position file \"{}\" {} This version of Forklift never writes over it, so \
                    --position was not applied.",
                    path.to_string_lossy(), reason
                )),
                None => Ok(None),
            },
            Err(PositionUnread::Damaged(reason)) => reason,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "is missing.".to_string(),
        Err(e) => format!("cannot be read: {}.", e),
    };

    let (position, from) = match chosen {
        Some(pallet) => (chosen_position(pallet)?, PositionSource::Chosen),
        None => match pallet_utils::read_position_copy_in(&dir)? {
            Some(copy) => (copy, PositionSource::Copy),
            None => match operation_utils::read()? {
                Some(operation) => (operation.source, PositionSource::Operation),
                None => return Err(no_evidence(&path, &unread)),
            },
        },
    };

    pallet_utils::write_position(&position)?;

    Ok(Some(PositionRebuilt { pallet: position.pallet, base: position.base, from }))
}

/// A position that reads: `--position` is refused over it, and its copy is rewritten if it does
/// not match.
fn keep(dir: &std::path::Path, position: BayPosition, chosen: Option<&str>) -> Result<Option<PositionRebuilt>, String> {
    if chosen.is_some() {
        return Err(format!(
            "The bay's position reads: it stands on pallet \"{}\". --position sets only a position that \
            is lost or damaged, so nothing was changed; \"forklift shift\" moves a bay to another pallet.",
            position.pallet
        ));
    }

    if pallet_utils::read_position_copy_in(dir)?.as_ref() != Some(&position) {
        pallet_utils::write_position_copy_in(dir, &position)?;
    }

    Ok(None)
}

/// The position `--position <pallet>` sets: the pallet at its head, or unborn when the warehouse
/// has no user pallet at all.
fn chosen_position(pallet: &str) -> Result<BayPosition, String> {
    pallet_utils::validate_pallet_name(pallet)?;

    match pallet_utils::get_pallet_head(pallet)? {
        Some(head) => Ok(BayPosition::new(pallet, Some(head))),
        None if pallet_utils::list_pallets()?.is_empty() => Ok(BayPosition::new(pallet, None)),
        None => Err(format!(
            "Pallet \"{}\" does not exist, so the bay cannot be put on it; nothing was changed. The \
            pallets are: {}.",
            pallet, quoted(&pallet_utils::list_pallets()?)
        )),
    }
}

/// The refusal when nothing says exactly where the bay stood.
fn no_evidence(path: &std::path::Path, reason: &str) -> String {
    let pallets = pallet_utils::list_pallets().unwrap_or_default();

    let hint = journal_utils::peek().ok().flatten()
        .map(|entry| entry.after.current_pallet)
        .map(|pallet| format!(
            " The undo journal last recorded the bay on pallet \"{}\", but other commands move a bay \
            without journaling, so that is only a hint.",
            pallet
        ))
        .unwrap_or_default();

    let known = match pallets.is_empty() {
        true => " The warehouse has no pallets yet.".to_string(),
        false => format!(" The pallets are: {}.", quoted(&pallets)),
    };

    CoreError::refusal(
        RefusalCode::BayPositionDamaged,
        format!(
            "The bay position file \"{}\" {} It has no readable copy and no operation was left \
            unfinished, so where the bay stood cannot be told; nothing was changed.{}{}",
            path.to_string_lossy(), reason, known, hint
        ),
        "forklift heal --position <pallet>",
    ).into()
}

/// `names`, each quoted, comma-separated.
fn quoted(names: &[String]) -> String {
    names.iter().map(|name| format!("\"{}\"", name)).collect::<Vec<_>>().join(", ")
}
