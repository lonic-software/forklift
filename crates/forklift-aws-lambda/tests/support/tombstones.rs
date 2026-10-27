//! The tombstone half of the [`RefStore`] contract, run against both stores: `protocol.rs` runs it
//! on [`MemoryRefStore`](forklift_aws_lambda::memory::MemoryRefStore), `aws_integration.rs` on
//! `DynamoRefStore`. One body, so the fake cannot drift from the conditions the real store writes.

use forklift_aws_lambda::reissue::request_token;
use forklift_aws_lambda::store::{CasOutcome, ExpectedHead, OfficePrecondition, RefHead, RefStore};
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::PalletNamespace::{Meta, User};

/// Drive a fresh, untrusted, office-less store through a delete and back.
pub fn upholds_the_tombstone_contract(refs: &impl RefStore) {
    let one = "1".repeat(64);
    let two = "2".repeat(64);
    let office = "3".repeat(64);
    let cas = |expected, new: &str| {
        refs.compare_and_set_head(User, "feature", expected, new, OfficePrecondition::NotConsumed, None, &request_token())
            .expect("cas")
    };
    let delete = |old: &str, office_head| {
        refs.delete_head(User, "feature", old, office_head, None, &request_token()).expect("delete")
    };

    assert_eq!(cas(ExpectedHead::Unborn, &one), CasOutcome::Committed);

    // A delete from a head the pallet is not at moves nothing.
    assert_eq!(
        delete(&two, OfficePrecondition::NotConsumed),
        CasOutcome::Conflict { current: RefHead::Live(one.clone()) }
    );

    // Conditioned on an unborn office, which it is: the delete lands, and the entry is now a
    // tombstone of the head it held.
    assert_eq!(delete(&one, OfficePrecondition::Unborn), CasOutcome::Committed);
    assert_eq!(refs.get_head(User, "feature").expect("get"), RefHead::Deleted(one.clone()));

    let listing = refs.list_refs().expect("list");
    assert!(listing.live.iter().all(|(pallet, _)| pallet.name != "feature"), "{:?}", listing);
    assert_eq!(
        listing.tombstones.iter().map(|(pallet, head)| (pallet.to_wire(), head.clone())).collect::<Vec<_>>(),
        vec![("feature".to_string(), one.clone())]
    );

    // A creation that expects no entry at all refuses the tombstone — the item is kept, so a
    // condition on the item (not the head) fails. An update, and a second delete, see it too.
    assert_eq!(cas(ExpectedHead::Unborn, &two), CasOutcome::Conflict { current: RefHead::Deleted(one.clone()) });
    assert_eq!(cas(ExpectedHead::At(&one), &two), CasOutcome::Conflict { current: RefHead::Deleted(one.clone()) });
    assert_eq!(
        delete(&one, OfficePrecondition::NotConsumed),
        CasOutcome::Conflict { current: RefHead::Deleted(one.clone()) }
    );

    // A re-creation acknowledging some other tombstone moves nothing; the right one lifts the
    // pallet back and clears the tombstone.
    assert_eq!(cas(ExpectedHead::Deleted(&two), &two), CasOutcome::Conflict { current: RefHead::Deleted(one.clone()) });
    assert_eq!(cas(ExpectedHead::Deleted(&one), &two), CasOutcome::Committed);
    assert_eq!(refs.get_head(User, "feature").expect("get"), RefHead::Live(two.clone()));
    assert!(refs.list_refs().expect("list").tombstones.is_empty());
    assert_eq!(cas(ExpectedHead::Deleted(&one), &one), CasOutcome::Conflict { current: RefHead::Live(two.clone()) });

    // Once an office exists, a delete conditioned on it being unborn is refused for the office,
    // and one conditioned on its head lands.
    assert_eq!(
        refs.compare_and_set_head(Meta, OFFICE_PALLET_NAME, ExpectedHead::Unborn, &office, OfficePrecondition::NotConsumed, None, &request_token())
            .expect("office"),
        CasOutcome::Committed
    );
    assert_eq!(delete(&two, OfficePrecondition::Unborn), CasOutcome::OfficeMoved { current: Some(office.clone()) });
    assert_eq!(refs.get_head(User, "feature").expect("get"), RefHead::Live(two.clone()));
    assert_eq!(delete(&two, OfficePrecondition::At(&office)), CasOutcome::Committed);
    assert_eq!(refs.get_head(User, "feature").expect("get"), RefHead::Deleted(two));
}
