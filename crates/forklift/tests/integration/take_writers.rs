//! Every production writer of a pallet ref, and why each keeps a take's known-verified set sound.
//!
//! A take judged against a held anchor skips the history of every local user-pallet head: each
//! was signed here or verified by a take, so its ancestry needs no second walk. That holds only
//! while every writer of a user-pallet ref does one of the things listed in [`Class`]. This test
//! finds every call of a ref-write primitive outside test code and fails when the set changes, so
//! a new writer is classified before it lands. The server and the Lambda head write the remote's
//! refs, not a warehouse's, and are not scanned.

use crate::source_scan::{calls, differences, production_sites};

/// Why a writer keeps the known-verified set sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// Writes a parcel it signed itself while an anchor is held.
    Signs,
    /// Writes a head this warehouse already held: another local pallet's, or one an operation it
    /// undoes replaced.
    CopiesLocal,
    /// Verifies the head against the held anchor and office, and writes under both.
    Verifies,
    /// Writes only while no anchor is held.
    RefusesWhenAnchored,
    /// Writes a meta pallet, which no take treats as verified.
    Meta,
    /// A primitive whose own callers are listed.
    Primitive,
}

/// The ref-write primitives: a call of any of these writes a pallet ref.
const PRIMITIVES: &[&str] = &[
    "update_pallet_head",
    "adopt_pallet_head",
    "update_pallet_head_in",
    "put_back_pallet_head_in",
    "compensating_head_write",
    "update_pallet_head_verified",
    "update_pallet_head_locked",
    "write_head_or_roll_back",
    "recreate_pallet_head",
    "write_head_locked",
    "repair_pallet_head",
];

/// Every production writer: the file, the function holding the call, and why it is sound. A
/// function holding several calls is listed once per call.
const WRITERS: &[(&str, &str, Class)] = &[
    // Signed whenever an anchor is held: an unsigned head is written only under the unsigned
    // writer's preconditions, no anchor and no office.
    ("crates/forklift-core/src/util/deliver_utils.rs", "deliver", Class::Signs),
    // An undone delete's head is verified as the anchor and office stand now.
    ("crates/forklift-core/src/util/journal_utils.rs", "create_deleted", Class::Verifies),
    ("crates/forklift-core/src/util/journal_utils.rs", "put_back_refs", Class::CopiesLocal),
    ("crates/forklift-core/src/util/journal_utils.rs", "un_restore_current", Class::CopiesLocal),
    ("crates/forklift-core/src/util/office_utils.rs", "write_trust_office", Class::Meta),
    // A lower rolled forward under the preconditions its take was verified against, or a
    // consolidation's fast-forward to a local head.
    ("crates/forklift-core/src/util/operation_utils.rs", "recover_operation", Class::Verifies),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_pallet_head", Class::Primitive),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_meta_pallet_head", Class::Meta),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_pallet_head_checked", Class::Primitive),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_pallet_head_locked", Class::Primitive),
    // A rebuilt or chosen head of a damaged ref is verified as the anchor and office stand now, as
    // undo's is.
    ("crates/forklift-core/src/util/ref_heal.rs", "repair", Class::Verifies),
    ("crates/forklift-core/src/util/ref_heal.rs", "set_damaged_ref", Class::Verifies),
    ("crates/forklift-core/src/util/shift_utils.rs", "write_head_or_roll_back", Class::Primitive),
    ("crates/forklift-core/src/util/shift_utils.rs", "write_head_or_roll_back", Class::Primitive),
    // Signed whenever an anchor is held; a consolidation's merged head is verified first.
    ("crates/forklift-core/src/util/stack_utils.rs", "write_head", Class::Signs),
    // A restored tombstone's head is verified as the anchor and office stand now, as undo's is.
    ("crates/forklift-core/src/util/tombstone_utils.rs", "restore_pallet", Class::Verifies),
    ("crates/forklift/src/commands/bay.rs", "add", Class::CopiesLocal),
    ("crates/forklift/src/commands/consolidate.rs", "fast_forward", Class::CopiesLocal),
    // With --repair it writes a repair of the verified head instead, signed as a stack is, under
    // the same preconditions.
    ("crates/forklift/src/commands/clone.rs", "franchise_into_prepared_target", Class::Verifies),
    ("crates/forklift/src/commands/import_git.rs", "handle_command", Class::RefusesWhenAnchored),
    ("crates/forklift/src/commands/lower.rs", "lower_working_pallet", Class::Verifies),
    // A named lower's write: the verified head, or a repair of it signed as a stack is, under the
    // preconditions the head was verified with.
    ("crates/forklift/src/commands/lower.rs", "write_named_head", Class::Verifies),
    // A repair is signed as a stack is, on a head verified under the preconditions it is written with.
    ("crates/forklift/src/commands/lower.rs", "repair_pallet", Class::Signs),
    // At a hash, verified; at another pallet, that pallet's head.
    ("crates/forklift/src/commands/pallet.rs", "create_pallet", Class::Verifies),
    ("crates/forklift/src/commands/pallet.rs", "create_pallet", Class::CopiesLocal),
];

/// Every call of a ref-write primitive outside test code, as (file, function), once per call.
fn found_writers() -> Vec<(String, String)> {
    production_sites(&["crates/forklift/src", "crates/forklift-core/src"], |line| {
        PRIMITIVES.iter().filter(|primitive| calls(line, primitive)).count()
    })
}

/// The production writers of a pallet ref are exactly the classified ones. Revert: add a call of
/// `update_pallet_head` to any production function (or remove one), and this fails naming it.
#[test]
fn every_pallet_ref_writer_is_classified() {
    let expected: Vec<(&str, &str)> = WRITERS.iter().map(|(file, function, _)| (*file, *function)).collect();
    let (unlisted, gone) = differences(&found_writers(), &expected);

    assert!(
        unlisted.is_empty() && gone.is_empty(),
        "The writers of a pallet ref changed. Classify each new one in WRITERS: a take skips the \
        history of every local user-pallet head, which is sound only while each writer signs, \
        copies a local head, verifies, or refuses while an anchor is held.\nFound but not listed: \
        {:#?}\nListed but not found: {:#?}",
        unlisted, gone
    );
}
