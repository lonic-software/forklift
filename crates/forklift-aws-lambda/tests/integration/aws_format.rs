//! What holds the AWS head's format still: the DynamoDB items each format writes, frozen per
//! [`AWS_FORMAT`] on the memory fake (docs/format/WAREHOUSE_FORMAT.md, "The fixtures").
//! `aws_integration.rs` checks that DynamoDB stores the same items.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use forklift_aws_lambda::format::{gate, upgrade, UpgradeOptions, AWS_FORMAT, CHAIN};
use forklift_aws_lambda::memory::MemoryRefStore;
use forklift_aws_lambda::store::{FormatFence, RefHead, RefStore};
use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::PalletNamespace;

#[path = "../support/aws_items.rs"]
mod aws_items;

use aws_items::{fixtures_root, frozen_items, hash, items_path, populate, read_items};

/// The fixture warehouse, on the memory store.
fn populated() -> MemoryRefStore {
    let refs = MemoryRefStore::new();
    populate(&refs);
    refs
}

/// Each `aws-format-<N>` folder of fixtures, by `N`.
fn fixture_formats() -> BTreeMap<u32, PathBuf> {
    std::fs::read_dir(fixtures_root()).unwrap()
        .map(|entry| entry.unwrap().path())
        .filter_map(|path| {
            let format = path.file_name()?.to_str()?.strip_prefix("aws-format-")?.parse().ok()?;
            Some((format, path.join("items")))
        })
        .collect()
}

fn write_items(items: &[(String, String, String)]) -> String {
    let mut text = format!("# The DynamoDB items AWS format {} writes. Frozen: never edit this file.\n", AWS_FORMAT);
    for (entity, attribute, value) in items {
        text.push_str(&format!("{}\t{}\t{}\n", entity, attribute, value));
    }
    text
}

/// This build writes exactly the items its format's fixture froze, so no item, attribute, sort key,
/// state or anchor field can change without an AWS format bump.
///
/// Revert: rename an attribute, change the format state's text, or add an anchor field → the
/// assertion shows the item.
#[test]
fn this_build_writes_the_frozen_items() {
    let frozen = frozen_items();
    let written = populated().items();

    assert_eq!(written, frozen,
        "the items this build writes differ from AWS format {}'s, which never change. A changed item is a \
        format break: bump AWS_FORMAT and ship an upgrade step.", AWS_FORMAT);
}

/// This build serves the warehouse its format's fixture froze, and reads every item back.
///
/// Revert: make the gate refuse its own state, or the anchor fail to decode → the assertion.
#[test]
fn this_build_serves_the_frozen_items() {
    let refs = MemoryRefStore::from_items(&frozen_items()).unwrap();
    assert_eq!(gate(&refs).unwrap(), FormatFence::At(AWS_FORMAT.to_string()));

    assert_eq!(refs.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Live(hash('1')));
    assert_eq!(refs.get_head(PalletNamespace::User, "gone").unwrap(), RefHead::Deleted(hash('2')));
    assert_eq!(refs.get_head(PalletNamespace::Meta, OFFICE_PALLET_NAME).unwrap(), RefHead::Live(hash('3')));

    let listing = refs.list_refs().unwrap();
    assert_eq!((listing.live.len(), listing.tombstones.len()), (2, 1));

    let (anchor, _) = refs.get_trust().unwrap().expect("the anchor reads back");
    assert_eq!(TrustAnchorDto::from(&anchor).genesis, hash('4'));
}

/// Every older AWS format's fixture the chain reaches upgrades through it and is then served; one
/// below the chain's floor is refused by both, loudly, and left as it was. Format 1 shared its
/// object keys between warehouses, so nothing reaches it.
///
/// Revert: let the gate serve format 1, or the upgrade stamp it → red.
#[test]
fn older_aws_fixtures_upgrade_and_are_served() {
    let options = UpgradeOptions { owner: "fixture".to_string(), takeover: false, wait: Duration::ZERO };

    for (format, path) in fixture_formats() {
        assert!(format <= AWS_FORMAT, "a fixture of AWS format {} is newer than this build's {}", format, AWS_FORMAT);
        if format == AWS_FORMAT {
            continue;
        }

        let refs = MemoryRefStore::from_items(&read_items(&path)).unwrap();
        if format < CHAIN.floor() {
            let refusal = gate(&refs).unwrap_err();
            assert!(refusal.message.contains("Make it again under a new id"), "AWS format {}: {}", format, refusal.message);
            let error = upgrade(&refs, &CHAIN, "fixture", &options).unwrap_err();
            assert!(error.contains("Make it again under a new id"), "AWS format {}: {}", format, error);
            assert_eq!(refs.read_format().unwrap(), Some(format.to_string()), "AWS format {}", format);
            continue;
        }

        upgrade(&refs, &CHAIN, "fixture", &options).unwrap_or_else(|e| panic!("AWS format {}: {}", format, e));
        assert_eq!(gate(&refs).unwrap(), FormatFence::At(AWS_FORMAT.to_string()), "AWS format {}", format);
    }

    assert!(fixture_formats().contains_key(&1), "the format 1 fixture is what proves it is refused");
}

/// Make the current AWS format's fixture. Run it once per format, after a bump:
/// `cargo test -p forklift-aws-lambda --test integration -- --ignored freeze`. It refuses a format
/// that already has one, since it never changes.
#[test]
#[ignore]
fn freeze_the_current_aws_format_fixture() {
    let path = items_path(AWS_FORMAT);
    assert!(!path.exists(), "{} is already frozen", path.display());

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, write_items(&populated().items())).unwrap();
}
