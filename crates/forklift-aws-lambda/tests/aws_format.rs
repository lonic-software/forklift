//! What holds the AWS head's format still: the DynamoDB items each format writes, frozen per
//! [`AWS_FORMAT`] on the memory fake (docs/format/WAREHOUSE_FORMAT.md, "The fixtures").

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use forklift_aws_lambda::format::{gate, upgrade, UpgradeOptions, AWS_FORMAT, CHAIN};
use forklift_aws_lambda::memory::MemoryRefStore;
use forklift_aws_lambda::store::{
    CasOutcome, ExpectedHead, FormatFence, OfficePrecondition, RefHead, RefStore, TrustOutcome,
};
use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::PalletNamespace;

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn items_path(format: u32) -> PathBuf {
    fixtures_root().join(format!("aws-format-{}", format)).join("items")
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

/// The items a fixture file lists, one per line: sort key, attribute and value, tab-separated.
fn read_items(path: &PathBuf) -> Vec<(String, String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("no AWS fixture at {}: {}", path.display(), e))
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let mut field = || fields.next().unwrap_or_else(|| panic!("an item line has three fields: {}", line)).to_string();
            (field(), field(), field())
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

fn hash(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// A warehouse holding every kind of item: the format, a live pallet, a deleted one, the office
/// pallet and the trust anchor, each written through the store's own write paths.
fn populate() -> MemoryRefStore {
    let refs = MemoryRefStore::new();
    let fence = || gate(&refs).expect("the fixture warehouse is served");
    let set = |namespace, name, new: &str| {
        let outcome = refs.compare_and_set_head(
            namespace, name, ExpectedHead::Unborn, new, OfficePrecondition::NotConsumed, None, &fence(), "fixture",
        ).unwrap();
        assert!(matches!(outcome, CasOutcome::Committed), "{:?}", outcome);
    };

    set(PalletNamespace::User, "main", &hash('1'));
    set(PalletNamespace::User, "gone", &hash('2'));
    set(PalletNamespace::Meta, OFFICE_PALLET_NAME, &hash('3'));

    let deleted = refs.delete_head(
        PalletNamespace::User, "gone", &hash('2'), OfficePrecondition::NotConsumed, None, &fence(), "fixture",
    ).unwrap();
    assert!(matches!(deleted, CasOutcome::Committed), "{:?}", deleted);

    let anchor = TrustAnchorDto {
        genesis: hash('4'),
        enabled_at: 1_780_000_000,
        boundary: vec![hash('5')],
        prior_genesis: Some(hash('6')),
        adopts: Some(hash('7')),
    };
    let trust = refs.put_trust_if_absent(&anchor.to_anchor(), &fence(), "fixture").unwrap();
    assert!(matches!(trust, TrustOutcome::Established), "{:?}", trust);

    refs
}

/// This build writes exactly the items its format's fixture froze, so no item, attribute, sort key,
/// state or anchor field can change without an AWS format bump.
///
/// Revert: rename an attribute, change the format state's text, or add an anchor field → the
/// assertion shows the item.
#[test]
fn this_build_writes_the_frozen_items() {
    let frozen = read_items(&items_path(AWS_FORMAT));
    let written = populate().items();

    assert_eq!(written, frozen,
        "the items this build writes differ from AWS format {}'s, which never change. A changed item is a \
        format break: bump AWS_FORMAT and ship an upgrade step.", AWS_FORMAT);
}

/// This build serves the warehouse its format's fixture froze, and reads every item back.
///
/// Revert: make the gate refuse its own state, or the anchor fail to decode → the assertion.
#[test]
fn this_build_serves_the_frozen_items() {
    let refs = MemoryRefStore::from_items(&read_items(&items_path(AWS_FORMAT))).unwrap();
    assert_eq!(gate(&refs).unwrap(), FormatFence::At(AWS_FORMAT.to_string()));

    assert_eq!(refs.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Live(hash('1')));
    assert_eq!(refs.get_head(PalletNamespace::User, "gone").unwrap(), RefHead::Deleted(hash('2')));
    assert_eq!(refs.get_head(PalletNamespace::Meta, OFFICE_PALLET_NAME).unwrap(), RefHead::Live(hash('3')));

    let listing = refs.list_refs().unwrap();
    assert_eq!((listing.live.len(), listing.tombstones.len()), (2, 1));

    let (anchor, _) = refs.get_trust().unwrap().expect("the anchor reads back");
    assert_eq!(TrustAnchorDto::from(&anchor).genesis, hash('4'));
}

/// Every older AWS format's fixture upgrades through this build's chain and is then served. Format
/// 1 is the first, so there is none yet.
#[test]
fn older_aws_fixtures_upgrade_and_are_served() {
    for (format, path) in fixture_formats() {
        assert!(format <= AWS_FORMAT, "a fixture of AWS format {} is newer than this build's {}", format, AWS_FORMAT);
        if format == AWS_FORMAT {
            continue;
        }

        let refs = MemoryRefStore::from_items(&read_items(&path)).unwrap();
        let options = UpgradeOptions { owner: "fixture".to_string(), takeover: false, wait: Duration::ZERO };
        upgrade(&refs, &CHAIN, "fixture", &options).unwrap_or_else(|e| panic!("AWS format {}: {}", format, e));
        assert_eq!(gate(&refs).unwrap(), FormatFence::At(AWS_FORMAT.to_string()), "AWS format {}", format);
    }
}

/// Make the current AWS format's fixture. Run it once per format, after a bump:
/// `cargo test -p forklift-aws-lambda --test aws_format -- --ignored freeze`. It refuses a format
/// that already has one, since it never changes.
#[test]
#[ignore]
fn freeze_the_current_aws_format_fixture() {
    let path = items_path(AWS_FORMAT);
    assert!(!path.exists(), "{} is already frozen", path.display());

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, write_items(&populate().items())).unwrap();
}
