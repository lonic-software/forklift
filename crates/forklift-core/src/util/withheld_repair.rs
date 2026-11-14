//! The operator's repair of a served root's record of withheld content
//! (`forklift-server withhold repair`).
//!
//! A takedown is the host's act, recorded nowhere else, so a lost entry cannot be rebuilt: the
//! statement was the operator's words. What the repair can do is keep every takedown it can still
//! tell, and err toward withholding. It keeps each entry of the damaged record that still reads,
//! and from an entry that does not read whole but still opens with an object hash, it keeps that
//! hash withheld with [`PLACEHOLDER_STATEMENT`] and no chunk-list digest, for the operator to
//! restate with `withhold add`. Nothing is withheld on a guess beyond that.
//!
//! An entry lost whole cannot be told. A served head deletes its copy of withheld content, so a
//! lost entry whose copy is gone leaves an object that history names and nothing holds: the repair
//! walks the history the root keeps and lists each such object, for the operator to withhold again.
//! One whose copy is still stored is served again once the root is: no record shows it, so the
//! report says so.

use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::{RefLock, ServeLock};
use crate::util::sealed_utils::{self, Unrecorded};
use crate::util::withholding::{self, ChunkDigest, WithheldSet};
use crate::util::{object_utils, record_utils, tombstone_repair, tombstone_utils};

/// The statement a hash takes when the repair keeps it withheld from an entry that lost its own.
pub const PLACEHOLDER_STATEMENT: &str = "Withheld by this host; the statement was lost to a damaged record.";

/// What a repair did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repaired {
    /// Where the damaged record was moved to, if it was there.
    pub set_aside: Option<PathBuf>,
    /// How many entries still read whole and were kept.
    pub salvaged: usize,
    /// Each hash kept withheld with the placeholder statement, from an entry that did not read whole.
    pub placeholders: Vec<String>,
    /// How many lines held no object hash at all, left only in the moved-aside copy.
    pub unreadable_lines: usize,
    /// Each object the history kept here names that is absent and that no record holds: content a
    /// lost entry may have withheld, once its copy was deleted.
    pub absent: Vec<String>,
    /// Why the history could not be walked for absent objects, when it could not.
    pub walk_failed: Option<String>,
}

impl Repaired {
    /// The report, one line each.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();

        match &self.set_aside {
            Some(path) => lines.push(format!("Moved the damaged \"{}\" aside.", path.to_string_lossy())),
            None => lines.push("The record of withheld content was missing; wrote it anew.".to_string()),
        }

        lines.push(format!(
            "Kept {} entr{} of the old record{}.",
            self.salvaged,
            if self.salvaged == 1 { "y" } else { "ies" },
            match self.unreadable_lines {
                0 => String::new(),
                n => format!("; {} line{} named no object and {} only in the copy moved aside", n, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }),
            }
        ));

        for hash in &self.placeholders {
            lines.push(format!(
                "Kept {} withheld, though its entry did not read whole; restate its statement with \
                \"forklift-server withhold add {} <statement>\".",
                hash, hash
            ));
        }

        match (&self.walk_failed, self.absent.len()) {
            (Some(reason), _) => lines.push(format!("The history could not be walked for content a lost entry withheld: {}", reason)),
            (None, 0) => lines.push("No object the history here names is absent without a record holding it.".to_string()),
            (None, n) => lines.push(format!(
                "{} object{} the history here names {} absent, and no record holds {}: content a lost entry may \
                have withheld, once its copy was deleted. Withhold each one that was with \"forklift-server \
                withhold add <hash> <statement>\":",
                n, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }, if n == 1 { "it" } else { "them" }
            )),
        }

        lines.extend(self.absent.iter().map(|hash| format!("  {}", hash)));

        lines.push(
            "Content a lost entry withheld whose copy is still stored is served again once this warehouse is: \
            no record shows it. Withhold it again before serving if you know of any.".to_string()
        );

        lines
    }
}

/// Repair the record of withheld content of the warehouse at the current storage root (see the
/// module doc). Takes the serve lock, so it refuses while a server serves the root, then the ref
/// lock.
///
/// # Returns
/// * `Ok(Repaired)` - What was repaired.
/// * `Err(String)`  - The record reads, or is absent from a root never served, so there is nothing
///                    to repair; it is of a version this build does not know, which a newer
///                    Forklift repairs; the root is being served; or a file could not be read or
///                    written. Nothing was changed, unless the record's write failed after the
///                    damaged one was moved aside: the error then names the copy to put back.
pub fn repair_withheld() -> Result<Repaired, String> {
    let _serve_lock = ServeLock::acquire().map_err(|e| format!("Refusing to repair the record of withheld content: {}", e))?;
    let lock = RefLock::acquire()?;

    let path = globals::forklift_root().join("withheld");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    match &bytes {
        Some(bytes) => match withholding::parse(bytes) {
            Ok(_) => return Err("The record of withheld content reads whole; there is nothing to repair.".to_string()),
            Err(withholding::Unparsed::Record(record_utils::RecordError::OtherVersion(header))) => return Err(format!(
                "The record of withheld content is in a format this version of Forklift does not know (\"{}\"): \
                a newer Forklift wrote it, and repairs it.",
                header.escape_debug()
            )),
            Err(_) => {}
        },
        None if tombstone_utils::served_state()? == tombstone_utils::ServedState::Never => return Err(
            "This warehouse has never been served, so no record of withheld content reads as nothing withheld; \
            there is nothing to repair.".to_string()
        ),
        None => {}
    }

    let (set, salvaged, placeholders, unreadable_lines) = salvage(bytes.as_deref().unwrap_or_default());

    let set_aside = match bytes {
        Some(_) => Some(tombstone_repair::set_aside(&path)?),
        None => None,
    };

    set.write(&lock).map_err(|e| match &set_aside {
        Some(copy) => format!(
            "{} The damaged record was moved aside to \"{}\"; move it back to \"{}\" before repairing again.",
            e, copy.to_string_lossy(), path.to_string_lossy()
        ),
        None => e,
    })?;

    let (absent, walk_failed) = match absent_unheld() {
        Ok(absent) => (absent, None),
        Err(e) => (Vec::new(), Some(e)),
    };

    Ok(Repaired { set_aside, salvaged, placeholders, unreadable_lines, absent, walk_failed })
}

/// Every entry of a damaged record that still reads whole, and each hash kept with the
/// placeholder from an entry that does not but opens with one. A first line that is a record
/// header is skipped; an escaped entry reads as written, since a statement is one plain line.
fn salvage(bytes: &[u8]) -> (WithheldSet, usize, Vec<String>, usize) {
    let mut entries = std::collections::BTreeMap::new();
    let mut placeholders = Vec::new();
    let mut unreadable = 0;

    for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if raw.is_empty() || (index == 0 && raw.starts_with(b"/forklift-")) {
            continue;
        }

        // An entry is escaped as written; a line whose escape is broken is read raw, for its hash.
        let raw = String::from_utf8_lossy(raw);
        let line = record_utils::unescape(&raw).unwrap_or_else(|| raw.to_string());

        match withholding::parse_entry(&line) {
            Some((hash, statement, digest)) => {
                entries.insert(hash.to_string(), (statement.to_string(), digest));
            }
            None => match line.split(' ').next().filter(|hash| object_utils::is_object_hash(hash)) {
                Some(hash) if !entries.contains_key(hash) => {
                    entries.insert(hash.to_string(), (PLACEHOLDER_STATEMENT.to_string(), ChunkDigest::Unknown));
                    placeholders.push(hash.to_string());
                }
                Some(_) => {}
                None => unreadable += 1,
            },
        }
    }

    // A whole entry read after a broken one of the same hash wins over the placeholder.
    placeholders.retain(|hash| entries.get(hash).is_some_and(|(statement, _)| statement == PLACEHOLDER_STATEMENT));
    let salvaged = entries.len() - placeholders.len();
    let set = WithheldSet::of(entries.into_iter().map(|(hash, (statement, digest))| (hash, statement, digest)));

    (set, salvaged, placeholders, unreadable)
}

/// Every object the history this root keeps names that is absent and that no record holds, the
/// sealed record included, sorted.
fn absent_unheld() -> Result<Vec<String>, String> {
    let tolerated = sealed_utils::read_tolerated()?;
    let walked = sealed_utils::walk(&[], &tolerated, Unrecorded::Boundary)?
        .map_err(|damage| format!("object {} is absent, named by {}", damage.hash, damage.named_by))?;

    let mut absent: Vec<String> = walked.absent.into_iter().filter(|hash| !tolerated.sealed.contains(hash)).collect();
    absent.sort();

    Ok(absent)
}
