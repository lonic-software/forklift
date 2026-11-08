//! The warehouse the AWS item fixtures freeze, written through either store: `aws_format.rs` freezes
//! and checks it on [`MemoryRefStore`](forklift_aws_lambda::memory::MemoryRefStore), and
//! `aws_integration.rs` checks that `DynamoRefStore` writes the same items. One body, so the fixture
//! holds what DynamoDB stores, not only what the fake does.

use std::path::PathBuf;

use forklift_aws_lambda::format::{gate, AWS_FORMAT};
use forklift_aws_lambda::reissue::request_token;
use forklift_aws_lambda::head::redaction_record;
use forklift_aws_lambda::store::{
    CasOutcome, ExpectedHead, ItemKey, ItemWrite, OfficePrecondition, RedactionWrite, RedactionsPrecondition, RefStore, TrustOutcome,
};
use forklift_core::model::remote::TrustAnchorDto;
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::PalletNamespace;

pub fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn items_path(format: u32) -> PathBuf {
    fixtures_root().join(format!("aws-format-{}", format)).join("items")
}

/// The current AWS format's frozen items.
pub fn frozen_items() -> Vec<(String, String, String)> {
    read_items(&items_path(AWS_FORMAT))
}

/// The items a fixture file lists, one per line: sort key, attribute and value, tab-separated.
pub fn read_items(path: &PathBuf) -> Vec<(String, String, String)> {
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

pub fn hash(digit: char) -> String {
    digit.to_string().repeat(64)
}

/// Write a warehouse holding every kind of item: the format, a live pallet, a deleted one, the
/// office pallet, the trust anchor, an applied redaction's item, which records a holder, and the
/// chunk deletion's ledger and fence. Each
/// goes through the store's own write paths, each with its own request token as a Lambda's are
/// (DynamoDB refuses a token reused for a different transaction).
pub fn populate(refs: &impl RefStore) {
    let fence = || gate(refs).expect("the fixture warehouse is served");
    let set = |namespace, name, new: &str| {
        let outcome = refs.compare_and_set_head(
            namespace, name, ExpectedHead::Unborn, new, OfficePrecondition::NotConsumed, RedactionsPrecondition::NotConsumed, None, None, &fence(), &request_token(),
        ).unwrap();
        assert!(matches!(outcome, CasOutcome::Committed), "{:?}", outcome);
    };

    set(PalletNamespace::User, "main", &hash('1'));
    set(PalletNamespace::User, "gone", &hash('2'));
    set(PalletNamespace::Meta, OFFICE_PALLET_NAME, &hash('3'));

    let deleted = refs.delete_head(
        PalletNamespace::User, "gone", &hash('2'), OfficePrecondition::NotConsumed, None, &fence(), &request_token(),
    ).unwrap();
    assert!(matches!(deleted, CasOutcome::Committed), "{:?}", deleted);

    let anchor = TrustAnchorDto {
        genesis: hash('4'),
        enabled_at: 1_780_000_000,
        boundary: vec![hash('5')],
        prior_genesis: Some(hash('6')),
        adopts: Some(hash('7')),
    };
    let trust = refs.put_trust_if_absent(&anchor.to_anchor(), &fence(), &request_token()).unwrap();
    assert!(matches!(trust, TrustOutcome::Established), "{:?}", trust);

    let record = redaction_record(true, &[("gone".to_string(), hash('2'))], Some(1_780_000_001));
    let written = refs.put_redaction(&hash('8'), &record, None, &fence()).unwrap();
    assert!(matches!(written, RedactionWrite::Written), "{:?}", written);

    let (ledger, chunk_fence) = forklift_aws_lambda::head::chunk_fixture_items([&hash('9'), &hash('a'), &hash('b')]).unwrap();
    let writes = [
        ItemWrite { key: ItemKey::ChunkLedger, state: &ledger, expected: None },
        ItemWrite { key: ItemKey::ChunkFence, state: &chunk_fence, expected: None },
    ];
    let written = refs.put_items(&writes, &fence()).unwrap();
    assert!(matches!(written, RedactionWrite::Written), "{:?}", written);
}
