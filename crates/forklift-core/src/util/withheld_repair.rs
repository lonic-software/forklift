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
use crate::util::lock_utils::{RefLock, ServeLock};
use crate::util::sealed_utils::{self, Unrecorded};
use crate::util::withholding::{self, ChunkDigest, WithheldSet};
use crate::util::{applied_redactions, file_utils, object_utils, record_utils, tombstone_utils, unavailable};

/// The statement a hash takes when the repair keeps it withheld from an entry that lost its own.
pub const PLACEHOLDER_STATEMENT: &str = "Withheld by this host; the statement was lost to a damaged record.";

/// What a repair did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repaired {
    /// Where the damaged record was copied to, if it was there.
    pub set_aside: Option<PathBuf>,
    /// The record was there but did not match its digest, so an entry that reads whole may still
    /// have been altered in place, such as a flipped digit in its hash.
    pub unverified: bool,
    /// How many entries still read whole and were kept.
    pub salvaged: usize,
    /// Each hash kept withheld with the placeholder statement, from an entry that did not read whole.
    pub placeholders: Vec<String>,
    /// How many lines held no object hash at all, left only in the copy set aside.
    pub unreadable_lines: usize,
    /// Each file content (blob or recipe) the history kept here names that is absent and that no
    /// record holds: content a lost entry may have withheld, once its copy was deleted.
    pub absent: Vec<String>,
    /// Each absent tree and chunk no record holds: damage to the store, since a takedown never
    /// withholds one.
    pub absent_other: Vec<String>,
    /// The warehouse repaired, quoted for a command line.
    pub warehouse: String,
}

impl Repaired {
    /// The report, one line each.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();

        match &self.set_aside {
            Some(path) => lines.push(format!("Kept a copy of the damaged record at \"{}\".", path.to_string_lossy())),
            None => lines.push("The record of withheld content was missing; wrote it anew.".to_string()),
        }

        if self.unverified {
            lines.push(
                "The old record did not match its digest, so an entry kept because it reads whole may still have \
                been altered, such as a digit of its hash. Check the kept entries against your own record of \
                takedowns.".to_string()
            );
        }

        lines.push(format!(
            "Kept {} entr{} of the old record{}.",
            self.salvaged,
            if self.salvaged == 1 { "y" } else { "ies" },
            match self.unreadable_lines {
                0 => String::new(),
                n => format!("; {} line{} named no object and {} only in the copy set aside", n, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }),
            }
        ));

        for hash in &self.placeholders {
            lines.push(format!(
                "Kept {} withheld, though its entry did not read whole; restate its statement with \
                \"forklift-server withhold add --root {} {} --statement <statement>\".",
                hash, self.warehouse, hash
            ));
        }

        match self.absent.len() {
            0 => lines.push("No file the history here names is absent without a record holding it.".to_string()),
            n => lines.push(format!(
                "{} file{} the history here names {} absent, and no record holds {}: content a lost entry may \
                have withheld, once its copy was deleted. Withhold each one that was with \"forklift-server \
                withhold add --root {} <hash> --statement <statement>\":",
                n, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }, if n == 1 { "it" } else { "them" },
                self.warehouse
            )),
        }

        lines.extend(self.absent.iter().map(|hash| format!("  {}", hash)));

        if !self.absent_other.is_empty() {
            let n = self.absent_other.len();
            lines.push(format!(
                "{} folder{} or chunk{} the history here names {} absent too, which no takedown withholds: that is \
                damage to the store, not a lost entry. Restore {} from a copy of this warehouse:",
                n, if n == 1 { "" } else { "s" }, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" },
                if n == 1 { "it" } else { "them" }
            ));
            lines.extend(self.absent_other.iter().map(|hash| format!("  {}", hash)));
        }

        lines.push(
            "Content a lost entry withheld whose copy is still stored is served again once this warehouse is, \
            and content it withheld that was never stored here can be lifted and served: no record shows \
            either. Withhold any you know of again before serving.".to_string()
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
///                    to repair; it matches its digest and its header names a newer version, which
///                    a newer Forklift repairs; the root is being served; the history could not be
///                    walked; or a file could not be read or written. The damaged record is left in
///                    place until the repaired one replaces it, so a rerun finds it as it was.
///
/// A header damaged into a newer version's no longer matches the digest, so it is repaired as
/// damage: only a record whose digest verifies is taken as a newer version's.
pub fn repair_withheld() -> Result<Repaired, String> {
    let _serve_lock = ServeLock::acquire().map_err(|e| format!("Refusing to repair the record of withheld content: {}", e))?;
    let lock = RefLock::acquire()?;

    let path = withholding::withheld_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let mut unverified = false;
    match &bytes {
        Some(bytes) => match withholding::parse(bytes) {
            Ok(_) => return Err("The record of withheld content reads whole; there is nothing to repair.".to_string()),
            Err(withholding::Unparsed::Record(record_utils::RecordError::OtherVersion(header)))
                if withholding::written_by_newer(bytes) => return Err(format!(
                "The record of withheld content says a newer version of Forklift wrote it (\"{}\"), and it \
                matches its digest; that Forklift repairs it.",
                header.escape_debug()
            )),
            Err(withholding::Unparsed::Record(record_utils::RecordError::Damaged)) => {
                unverified = record_utils::has_digest_line(bytes);
            }
            Err(_) => {}
        },
        None if tombstone_utils::served_state()? == tombstone_utils::ServedState::Never => return Err(
            "This warehouse has never been served, so no record of withheld content reads as nothing withheld; \
            there is nothing to repair.".to_string()
        ),
        None => {}
    }

    let (set, salvaged, placeholders, unreadable_lines) =
        salvage(record_utils::without_digest_line(bytes.as_deref().unwrap_or_default()));

    // Everything is worked out before anything is written, so a repair that stops or fails leaves
    // the damaged record where it was, for a rerun to find whole.
    let absent = absent_unheld(&set).map_err(|e| format!(
        "Refusing to repair the record of withheld content: the history could not be walked for content a \
        lost entry withheld, which the report must list. Nothing was changed. {}",
        e
    ))?;

    // A copy aside, then the record replaced: never a moment without one.
    let set_aside = match &bytes {
        Some(bytes) => Some(file_utils::copy_aside(&path, bytes)?),
        None => None,
    };

    set.write(&lock)?;

    let warehouse = withholding::quoted_warehouse();
    let (absent, absent_other) = absent;

    Ok(Repaired { set_aside, unverified, salvaged, placeholders, unreadable_lines, absent, absent_other, warehouse })
}

/// Every entry of a damaged record that still reads whole, and each hash kept with the
/// placeholder from an entry that does not but opens with one. A first line that is a record
/// header is skipped; an escaped entry reads as written, since a statement is one plain line. A
/// line is whole only when it is valid UTF-8, its escapes read, and it ends in its newline: a
/// record cut inside its last entry, or a lost backslash, would otherwise keep an altered
/// statement as the takedown's words.
fn salvage(bytes: &[u8]) -> (WithheldSet, usize, Vec<String>, usize) {
    let mut entries = std::collections::BTreeMap::new();
    let mut placeholders = Vec::new();
    let mut unreadable = 0;

    let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    for (index, raw) in lines.iter().enumerate() {
        if raw.is_empty() || (index == 0 && raw.starts_with(b"/forklift-")) {
            continue;
        }

        // The piece after the last newline is a line the record was cut inside.
        let terminated = index + 1 < lines.len();
        let raw_text = String::from_utf8_lossy(raw);
        let unescaped = record_utils::unescape(&raw_text);
        let whole = terminated && std::str::from_utf8(raw).is_ok() && unescaped.is_some();

        // An entry is escaped as written; a line whose escape is broken is read raw, for its hash.
        let line = unescaped.unwrap_or_else(|| raw_text.to_string());

        match withholding::parse_entry(&line).filter(|_| whole) {
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

/// Every file content the history this root keeps names that is absent and that no record holds,
/// the sealed record and `withheld`, the salvaged record, included, sorted; and how many absent
/// trees and chunks no record holds, sorted.
fn absent_unheld(withheld: &WithheldSet) -> Result<(Vec<String>, Vec<String>), String> {
    let tolerated = sealed_utils::Tolerated {
        sealed: sealed_utils::read_sealed()?,
        applied: applied_redactions::read_applied()?,
        withheld: withheld.clone(),
        unavailable: unavailable::read_unavailable()?,
    };
    let walked = sealed_utils::walk(&[], &tolerated, Unrecorded::Boundary)?
        .map_err(|damage| format!("object {} is absent, named by {}", damage.hash, damage.named_by))?;

    let unheld = |hash: &&String| !tolerated.sealed.contains(hash);
    let mut absent: Vec<String> = walked.absent_files.iter().filter(unheld).cloned().collect();
    absent.sort();
    let mut other: Vec<String> = walked.absent.iter().filter(unheld).filter(|hash| !walked.absent_files.contains(*hash)).cloned().collect();
    other.sort();

    Ok((absent, other))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{self, StorageRootScope};
    use crate::util::pallet_utils;

    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-withheld-repair-test-{}-{}", name, std::process::id()));
            let _ = file_utils::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            // The walk reads the bay position, as on any warehouse a host serves.
            pallet_utils::write_position_in(&root.join(globals::FOLDER_NAME_FORKLIFT_ROOT),
                &pallet_utils::BayPosition::new("main", None)).unwrap();
            crate::util::park_utils::create_if_not_exists(&root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = file_utils::remove_dir_all(&self.root);
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const F: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn record() -> PathBuf {
        globals::forklift_root().join("withheld")
    }

    fn aside() -> Vec<PathBuf> {
        std::fs::read_dir(globals::forklift_root()).unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("withheld.damaged-"))
            .collect()
    }

    // Revert: drop the placeholder → a hash whose entry lost its statement is served again; keep
    // nothing of a broken record → every takedown in it is; or read a cut last line or a line of
    // bad UTF-8 as whole → a cut or garbled statement is kept as the takedown's words.
    #[test]
    fn a_damaged_record_keeps_every_hash_it_can_still_tell() {
        let _scratch = Scratch::new("salvage");
        assert!(tombstone_utils::mark_served(true).unwrap());

        // A whole entry, one cut inside its digest column, a line naming no object, one that
        // escapes a backslash, one of bad UTF-8, a second entry of the first hash, and no line
        // ending at the end: cut short inside its statement.
        let mut bytes = format!("/forklift-withheld 2\n{} - court order 7\n{} 12\nnot a hash at all\n{} - path a\\\\b\n", A, B, C).into_bytes();
        bytes.extend_from_slice(format!("{} - caf", E).as_bytes());
        bytes.extend_from_slice(b"\xff\n");
        bytes.extend_from_slice(format!("{} - path a\\b\n", F).as_bytes());
        bytes.extend_from_slice(format!("{} - DMCA notice\n{} - DM", A, D).as_bytes());
        std::fs::write(record(), &bytes).unwrap();
        assert!(withholding::is_damage(&withholding::read_withheld().unwrap_err()));

        let repaired = repair_withheld().unwrap();
        let set = withholding::read_withheld().unwrap();

        assert_eq!(set.statement(A), Some("DMCA notice"), "a whole entry read later is kept as written");
        assert_eq!(set.statement(B), Some(PLACEHOLDER_STATEMENT));
        assert_eq!(set.digest(B), Some(&ChunkDigest::Unknown));
        assert_eq!(set.statement(C), Some("path a\\b"), "read unescaped");
        assert_eq!(set.statement(E), Some(PLACEHOLDER_STATEMENT), "a statement of bad UTF-8 is not the operator's words");
        assert_eq!(set.statement(D), Some(PLACEHOLDER_STATEMENT), "a statement cut short is not the operator's words");
        assert_eq!(set.statement(F), Some(PLACEHOLDER_STATEMENT), "a statement whose escape lost its backslash is not either");
        assert_eq!((repaired.salvaged, repaired.placeholders.clone(), repaired.unreadable_lines),
                   (2, vec![B.to_string(), E.to_string(), F.to_string(), D.to_string()], 1));

        let moved = aside();
        assert_eq!(moved.len(), 1);
        assert_eq!(std::fs::read(&moved[0]).unwrap(), bytes, "the damaged bytes are kept aside");
        assert_eq!(repaired.set_aside.as_ref(), Some(&moved[0]));

        let report = repaired.report().join("\n");
        assert!(report.contains(&format!("forklift-server withhold add --root {} {} --statement <statement>", repaired.warehouse, B)), "{}", report);
        assert!(report.contains("served again"), "{}", report);

        assert!(repair_withheld().unwrap_err().contains("nothing to repair"), "a record that reads is left alone");
    }

    // Revert: read a missing record as nothing to repair → a served root wedged on the loss.
    #[test]
    fn a_lost_record_in_a_served_root_is_written_anew_and_a_root_never_served_is_left_alone() {
        let _scratch = Scratch::new("lost");
        assert!(repair_withheld().unwrap_err().contains("never been served"));
        assert!(!record().exists());

        assert!(tombstone_utils::mark_served(true).unwrap());
        file_utils::remove_file(record()).unwrap();

        let repaired = repair_withheld().unwrap();
        assert_eq!(repaired.set_aside, None);
        assert!(withholding::read_withheld().unwrap().is_empty());
        assert!(aside().is_empty());
    }

    // Revert: rewrite a record whose header names a newer version and matches its digest → a newer
    // Forklift's takedowns are rewritten in a format it did not choose; or take a header that no
    // longer matches the digest for a newer version's → a header damaged into one wedges the root,
    // the named repair refusing.
    #[test]
    fn a_newer_record_is_left_for_the_forklift_that_wrote_it_and_a_damaged_header_is_repaired() {
        let _scratch = Scratch::new("version");
        let newer = record_utils::to_bytes("withheld", 4, [format!("{} - statement", A)]);
        std::fs::write(record(), &newer).unwrap();

        let refusal = repair_withheld().unwrap_err();
        assert!(refusal.contains("a newer version of Forklift wrote it"), "{}", refusal);
        assert_eq!(std::fs::read(record()).unwrap(), newer);
        assert!(aside().is_empty());

        // The same header, damaged from version 3 into 4: it no longer matches the digest.
        let mut damaged = record_utils::to_bytes("withheld", 3, [format!("{} - statement", A)]);
        damaged["/forklift-withheld ".len()] = b'4';
        std::fs::write(record(), &damaged).unwrap();
        let repaired = repair_withheld().unwrap();
        assert_eq!((repaired.salvaged, repaired.unreadable_lines, repaired.unverified), (1, 0, true));
        assert_eq!(withholding::read_withheld().unwrap().statement(A), Some("statement"));

        // Undigested, and digested too: a header that matches its digest but names no newer
        // version is damage all the same. Revert: refuse every other version that matches its
        // digest → a digested "1" wedges the root.
        for header in ["/forklift-withheld 1", "/forklift-withheld ", "/forklift-withheld 2x", "/forklift-withheld 03", "/forklift-withheld +3"] {
            let bytes = format!("{}\n{} - statement\n", header, B).into_bytes();
            for bytes in [bytes.clone(), record_utils::with_digest(bytes)] {
                std::fs::write(record(), &bytes).unwrap();
                assert_eq!(repair_withheld().unwrap().salvaged, 1, "{:?} is damage, not a newer version", header);
                assert_eq!(withholding::read_withheld().unwrap().statement(B), Some("statement"));
            }
        }
    }

    // Revert: drop the caveat → the operator is told every kept entry is as written,
    // though one may hold a flipped hash; or salvage the digest line as an entry → it is counted
    // as a line naming no object.
    #[test]
    fn a_record_that_reads_whole_but_misses_its_digest_says_a_kept_entry_may_be_altered() {
        let _scratch = Scratch::new("altered");
        assert!(tombstone_utils::mark_served(true).unwrap());

        let mut bytes = record_utils::to_bytes("withheld", 3, [format!("{} - court order 7", A)]);
        bytes["/forklift-withheld 3\n".len()] = b'b';
        std::fs::write(record(), &bytes).unwrap();
        assert!(withholding::is_damage(&withholding::read_withheld().unwrap_err()));

        let repaired = repair_withheld().unwrap();
        assert_eq!((repaired.salvaged, repaired.unreadable_lines, repaired.unverified), (1, 0, true));
        let altered = format!("b{}", &A[1..]);
        assert_eq!(withholding::read_withheld().unwrap().statement(&altered), Some("court order 7"),
                   "the altered entry reads whole, so it is kept: the repair cannot tell");
        let report = repaired.report().join("\n");
        assert!(report.contains("did not match its digest") && report.contains("may still have been altered"), "{}", report);

        // A record from before the digest has none to fail, so nothing says an entry was altered.
        // Revert: take every damaged record for one that failed its digest → a false alarm over
        // every record written before the digest.
        std::fs::write(record(), record_utils::to_bytes("withheld", 2, [format!("{} - court order 7", A)])).unwrap();
        let repaired = repair_withheld().unwrap();
        assert_eq!((repaired.salvaged, repaired.unverified), (1, false));
        assert!(!repaired.report().join("\n").contains("did not match its digest"));
    }

    // Revert: take no serve lock → the record is rewritten under a server answering from it.
    #[test]
    fn the_repair_refuses_while_the_root_is_served() {
        let _scratch = Scratch::new("served");
        std::fs::write(record(), b"garbage").unwrap();

        let _serving = ServeLock::acquire().unwrap();
        assert!(repair_withheld().unwrap_err().starts_with("Refusing to repair the record of withheld content"));
        assert_eq!(std::fs::read(record()).unwrap(), b"garbage");
    }
}
