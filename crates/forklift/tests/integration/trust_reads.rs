//! Every production read of the trust file that can see a missing file as "no anchor held".
//!
//! [`read_trust_anchor`](forklift_core::util::office_utils::read_trust_anchor) refuses a missing
//! trust file when anything says trust was established here: the office pallet born, or the
//! file's copy standing. Two reads skip that judgement. The raw byte read returns `None` for a
//! missing file whatever stands beside it, and the adoption read ignores the office, so that
//! `forklift lower` can take a remote's anchor over a lost one. A caller that read either as
//! "never enrolled" would sign nothing, or judge a remote as first contact, under an office that
//! requires signatures. This test fails when the set of those callers changes, so each new one is
//! judged before it lands.

use crate::source_scan::{calls, differences, production_sites};

/// Where production code is read: the client, the core and the server.
const DIRS: &[&str] = &["crates/forklift/src", "crates/forklift-core/src", "crates/forklift-server/src"];

/// Every production call of the byte reads (the coded one and the raw file read beneath it), by
/// file and function, once per call.
const BYTE_READS: &[(&str, &str)] = &[
    // The read layer itself: the file before the witnesses are read, and again after one stood;
    // and the coded byte read over the raw one.
    ("crates/forklift-core/src/util/office_utils.rs", "read_trust_anchor_in"),
    ("crates/forklift-core/src/util/office_utils.rs", "read_trust_anchor_in"),
    ("crates/forklift-core/src/util/office_utils.rs", "read_trust_anchor_bytes"),
    // An install compares the bytes it would write and reads its own write back; it never reads
    // a missing file as permission to sign.
    ("crates/forklift-core/src/util/office_utils.rs", "install_trust"),
    ("crates/forklift-core/src/util/office_utils.rs", "install_trust"),
    // The copy's mend compares the trust file to the anchor its caller holds, and the copy to it;
    // a missing trust file writes nothing.
    ("crates/forklift-core/src/util/office_utils.rs", "mend_trust_copy"),
    ("crates/forklift-core/src/util/office_utils.rs", "mend_trust_copy"),
    // The check before a copy write reads only the copy, never the trust file.
    ("crates/forklift-core/src/util/office_utils.rs", "refuse_unwritable_copy"),
    // The first serve's check judges a trust file that stands; a missing one it leaves to the
    // coded read, which refuses it where trust was established.
    ("crates/forklift-core/src/util/office_utils.rs", "refuse_unsettled_trust"),
    // A ref write's anchor precondition compares the file's bytes to the ones the caller judged;
    // a missing file meets only a caller that read none, and is then read through the witnesses.
    ("crates/forklift-core/src/util/pallet_utils.rs", "check_preconditions"),
];

/// Every production construction of the adoption read: lower's check and its adoption, which
/// verify the office against the remote's anchor before writing it. The mode travels from them
/// into the adoption itself (`PreparedTrust::commit`: its install and its read-back), which names
/// no mode of its own.
const ADOPTS: &[(&str, &str)] = &[
    ("crates/forklift/src/commands/lower.rs", "handle_command"),
    ("crates/forklift/src/commands/lower.rs", "handle_command"),
];

/// How often `line` names the adoption variant, however it reaches it: through the enum, or bare
/// after an import of it. Its own declaration is not a construction.
fn adoptions(line: &str) -> usize {
    match line.trim() == "Adopt," {
        true => 0,
        false => words(line).filter(|word| *word == "Adopt").count(),
    }
}

/// An import of the variant counts as a use of it, as the enum path does; the declaration does not.
// Revert: count only the `TrustReadMode::Adopt` spelling → the import and the bare use are missed.
#[test]
fn the_adoption_read_is_found_however_it_is_named() {
    assert_eq!(adoptions("    check_remote_trust(&client, &info, None, TrustReadMode::Adopt)?;"), 1);
    assert_eq!(adoptions("use forklift_core::util::office_utils::TrustReadMode::{self, Adopt};"), 1);
    assert_eq!(adoptions("    let mode = Adopt;"), 1);
    assert_eq!(adoptions("    Adopt,"), 0);
    assert_eq!(adoptions("    let adopted = adopt_anchor(Adopted);"), 0);
}

/// The identifiers in `line`.
fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|word| !word.is_empty())
}

/// The production callers of the byte read and the constructions of the adoption read are
/// exactly the listed ones. Revert: call `read_trust_anchor_bytes` from another production
/// function, or pass `TrustReadMode::Adopt` from another caller, and this fails naming it.
#[test]
fn every_trust_read_that_can_skip_the_witnesses_is_listed() {
    let byte_reads = production_sites(DIRS, |line| {
        usize::from(calls(line, "read_trust_anchor_bytes")) + usize::from(calls(line, "read_trust_file"))
    });
    let adopts = production_sites(DIRS, adoptions);

    for (what, found, expected) in [("byte read", &byte_reads, BYTE_READS), ("adoption read", &adopts, ADOPTS)] {
        let (unlisted, gone) = differences(found, expected);
        assert!(
            unlisted.is_empty() && gone.is_empty(),
            "The production callers of the trust file's {} changed. It reads a missing trust file as \
            no anchor held, even where trust was established, so judge each new caller before \
            listing it.\nFound but not listed: {:#?}\nListed but not found: {:#?}",
            what, unlisted, gone
        );
    }
}
