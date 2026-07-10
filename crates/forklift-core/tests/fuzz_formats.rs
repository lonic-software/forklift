//! Format fuzzing + round-trip properties, part of the hardening test spine.
//!
//! Two guarantees, one file:
//!
//!  * **Never panic on malformed input.** Every object/inventory a client reads can come from
//!    untrusted storage (a remote's object store, a downloaded bundle) — the design rule is that
//!    downloads are *verifiable* offline, which is worthless if a crafted byte string aborts the
//!    process first. So a large corpus of mutated and random bytes is fed to every parse entry
//!    point under `catch_unwind`; a parse may return `Ok` or `Err`, but must never panic.
//!  * **Round-trip fidelity.** `parse(build(x))` reproduces `x` (in the builders' canonical form)
//!    for randomly generated valid objects of every format.
//!
//! The generator is a dependency-free, deterministically seeded splitmix64 — the repo has no
//! property-test crate, and a fixed seed keeps a discovered failure reproducible in CI (print the
//! offending bytes and the case is pinned as a regression test below). The specific crash inputs
//! this suite first found (huge length prefixes in the tree/inventory/parcel parsers; an
//! attacker-declared allocation length in bundle import and delta reconstruction) are pinned by
//! name in `mod regressions` so they stay fixed even if the random stream changes.

use forklift_core::builder::inventory::InventoryBuilder;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::enums::object::parsed_object::ParsedObject;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::blob::Blob;
use forklift_core::model::inventory::{Inventory, InventoryItem};
use forklift_core::model::operator::Operator;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::parser::inventory::inventory_parser::parse_inventory;
use forklift_core::parser::object::loose_object_parser::parse as parse_loose_object;
use forklift_core::util::byte_utils::number_to_vlq_bytes;

// ---------------------------------------------------------------------------------------------
// Deterministic RNG (splitmix64) — no external dependency, reproducible seed → stream.
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n` (0 when `n == 0`).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }

    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ---------------------------------------------------------------------------------------------
// Seed builders — valid serialized objects, via the real public builders.
// ---------------------------------------------------------------------------------------------

const SAMPLE_HASH: &str = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";

/// A small stable code for a dir-entry type — `DirEntryType` is `Copy + Eq` but not `Ord`/`Debug`,
/// so the round-trip checks compare and sort on this instead.
fn dir_code(item_type: DirEntryType) -> u8 {
    match item_type {
        DirEntryType::Normal => 1,
        DirEntryType::Executable => 2,
        DirEntryType::SymbolicLink => 3,
        DirEntryType::Tree => 4,
    }
}

/// A random valid-UTF-8 name drawn from a pool that includes the bytes file names may legally
/// carry (newline, end-of-text) and a multi-byte character, so mutation starts from realistic,
/// adversarial-but-legal shapes.
fn random_name(rng: &mut Rng) -> String {
    const TOKENS: [&str; 8] = ["a", "dir/", "with\nnewline", "with\u{3}eot", "📦", ".x", "-", "Z"];
    let parts = 1 + rng.below(4);
    (0..parts).map(|_| TOKENS[rng.below(TOKENS.len())]).collect()
}

fn random_hash(rng: &mut Rng) -> String {
    // Any ASCII-hex string works as an object address in these formats; a fixed length keeps the
    // generator simple while still exercising the read-until-terminator paths.
    (0..64).map(|_| char::from(b"0123456789abcdef"[rng.below(16)])).collect()
}

fn blob_object_bytes(rng: &mut Rng) -> Vec<u8> {
    let len = rng.below(64);
    let content = (0..len).map(|_| rng.byte()).collect();
    LooseObjectBuilder::build_blob(&Blob { content }).content
}

/// A tree with unique child names (the builder keys children by name, so duplicates collapse and
/// would break a naive round-trip count).
fn tree_seed(rng: &mut Rng) -> TreeItem {
    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut names = std::collections::HashSet::new();
    for _ in 0..rng.below(6) {
        let name = random_name(rng);
        if !names.insert(name.clone()) {
            continue;
        }
        let kind = match rng.below(3) {
            0 => DirEntryType::Normal,
            1 => DirEntryType::Executable,
            _ => DirEntryType::Tree,
        };
        tree.add_child(TreeItem::new(name, random_hash(rng), kind));
    }
    tree
}

fn tree_object_bytes(rng: &mut Rng) -> Vec<u8> {
    LooseObjectBuilder::build_tree(&tree_seed(rng)).content
}

fn parcel_seed(rng: &mut Rng) -> Parcel {
    let action = |rng: &mut Rng| ParcelAction {
        operator: Operator {
            identifier: "1a2b3c4d-0000-4000-8000-1234567890ab".to_string(),
            name: "ignored-on-the-wire".to_string(),
        },
        action: if rng.bool() { ParcelActionType::Author } else { ParcelActionType::Stack },
        // A description is either absent or non-empty: the builder normalizes `Some("")` to
        // `None`, so generating an empty string would make the round-trip check spuriously fail.
        description: if rng.bool() { None } else { Some(random_name(rng)) },
        timestamp: chrono::DateTime::from_timestamp(1_700_000_000 + rng.below(1000) as i64, 0).unwrap(),
    };

    Parcel {
        tree_hash: random_hash(rng),
        parents: (0..rng.below(4)).map(|_| random_hash(rng)).collect(),
        actions: (0..1 + rng.below(3)).map(|_| action(rng)).collect(),
        description: if rng.bool() { None } else { Some(random_name(rng)) },
    }
}

fn parcel_object_bytes(rng: &mut Rng) -> Vec<u8> {
    LooseObjectBuilder::build_parcel(&parcel_seed(rng)).content
}

fn inventory_seed(rng: &mut Rng) -> (Inventory, Vec<String>) {
    let mut inventory = Inventory::new();
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..rng.below(6) {
        let name = random_name(rng);
        if !seen.insert(name.clone()) {
            continue;
        }
        inventory.add_item(InventoryItem {
            metadata_change_timestamp: rng.next_u64(),
            content_change_timestamp: rng.next_u64(),
            device: rng.next_u64(),
            inode: rng.next_u64(),
            item_type: if rng.bool() { DirEntryType::Normal } else { DirEntryType::Executable },
            user_id: rng.below(2000) as u64,
            group_id: rng.below(2000) as u64,
            file_size: rng.next_u64(),
            hash: random_hash(rng),
            file_name_length: name.len() as u64,
            state: if rng.bool() { InventoryItemState::Normal } else { InventoryItemState::Deleted },
            name: name.clone(),
        });
        names.push(name);
    }
    (inventory, names)
}

fn inventory_bytes(rng: &mut Rng) -> Vec<u8> {
    InventoryBuilder::build(&inventory_seed(rng).0)
}

// ---------------------------------------------------------------------------------------------
// Mutators.
// ---------------------------------------------------------------------------------------------

/// The VLQ encoding of `u64::MAX` — the shape that turned a `start + length` into an overflow and
/// a `vec![0u8; length]` into a capacity-overflow. Splicing it in at random positions is what
/// makes the fuzzer reliably land on the length-prefix bugs.
fn huge_vlq() -> Vec<u8> {
    number_to_vlq_bytes(u64::MAX)
}

fn mutate(bytes: &mut Vec<u8>, rng: &mut Rng) {
    let mutations = 1 + rng.below(4);
    for _ in 0..mutations {
        if bytes.is_empty() {
            bytes.push(rng.byte());
            continue;
        }
        match rng.below(6) {
            // Truncate at a random point (short/partial reads).
            0 => {
                let at = rng.below(bytes.len());
                bytes.truncate(at);
            }
            // Flip a random bit.
            1 => {
                let at = rng.below(bytes.len());
                bytes[at] ^= 1 << rng.below(8);
            }
            // Overwrite a run with random bytes.
            2 => {
                let start = rng.below(bytes.len());
                let end = start + rng.below(bytes.len() - start + 1);
                for b in &mut bytes[start..end] {
                    *b = rng.byte();
                }
            }
            // Splice in a huge VLQ (targets every length-prefix reader).
            3 => {
                let at = rng.below(bytes.len() + 1);
                let payload = huge_vlq();
                bytes.splice(at..at, payload);
            }
            // Zero a run (a length/sentinel of zero flips control flow).
            4 => {
                let start = rng.below(bytes.len());
                let end = start + rng.below(bytes.len() - start + 1);
                for b in &mut bytes[start..end] {
                    *b = 0;
                }
            }
            // Grow with random trailing bytes.
            _ => {
                for _ in 0..rng.below(16) {
                    bytes.push(rng.byte());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The never-panic property.
// ---------------------------------------------------------------------------------------------

/// A named parse entry point: a label and a function that parses (discarding the result — a parse
/// error is fine here, only a panic is the bug).
type ParseEntryPoint = (&'static str, fn(&[u8]));

/// Feed one byte string to every parser under `catch_unwind`. A parse result of `Ok` or `Err` is
/// equally acceptable; a panic is the bug. Returns the offending description on panic so the
/// caller can print the reproducing bytes.
fn parse_never_panics(bytes: &[u8]) -> Result<(), String> {
    let entry_points: [ParseEntryPoint; 2] = [
        ("loose_object", |b| {
            let _ = parse_loose_object(b);
        }),
        ("inventory", |b| {
            let _ = parse_inventory(b);
        }),
    ];

    for (label, parse) in entry_points {
        let owned = bytes.to_vec();
        let result = std::panic::catch_unwind(move || parse(&owned));
        if result.is_err() {
            return Err(format!("parser `{}` panicked", label));
        }
    }
    Ok(())
}

#[test]
fn parsers_never_panic_on_mutated_or_random_input() {
    // A fixed base seed keeps the whole run reproducible; the inner streams vary per case.
    let mut rng = Rng::new(0xF01C_11F7_5EED_0001);

    let generators: [fn(&mut Rng) -> Vec<u8>; 4] =
        [blob_object_bytes, tree_object_bytes, parcel_object_bytes, inventory_bytes];

    let mut cases = 0usize;

    // Mutated valid objects: start from a real object and corrupt it, so the fuzzer reaches deep
    // into each parser instead of bouncing off the version/header check.
    for _ in 0..4000 {
        for generate in generators {
            let mut bytes = generate(&mut rng);
            mutate(&mut bytes, &mut rng);
            if let Err(what) = parse_never_panics(&bytes) {
                panic!("{what} on mutated input: {:02x?}", bytes);
            }
            cases += 1;
        }
    }

    // Purely random byte strings of assorted lengths (catches header-level arithmetic).
    for _ in 0..4000 {
        let len = rng.below(256);
        let bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        if let Err(what) = parse_never_panics(&bytes) {
            panic!("{what} on random input: {:02x?}", bytes);
        }
        cases += 1;
    }

    assert!(cases >= 20_000, "the fuzzer should exercise a large corpus, ran {cases}");
}

// ---------------------------------------------------------------------------------------------
// Round-trip properties: parse(build(x)) == x (canonical form).
// ---------------------------------------------------------------------------------------------

#[test]
fn blobs_round_trip() {
    let mut rng = Rng::new(0xB10B_0002);
    for _ in 0..500 {
        let len = rng.below(128);
        let content: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        let bytes = LooseObjectBuilder::build_blob(&Blob { content: content.clone() }).content;
        match parse_loose_object(&bytes).unwrap() {
            ParsedObject::Blob(blob) => assert_eq!(blob.content, content),
            other => panic!("a blob parsed as {:?}", std::mem::discriminant(&other)),
        }
    }
}

#[test]
fn trees_round_trip() {
    let mut rng = Rng::new(0x77EE_0003);
    for _ in 0..500 {
        let tree = tree_seed(&mut rng);
        let bytes = LooseObjectBuilder::build_tree(&tree).content;
        let parsed = match parse_loose_object(&bytes).unwrap() {
            ParsedObject::Tree(t) => t,
            other => panic!("a tree parsed as {:?}", std::mem::discriminant(&other)),
        };

        let mut want: Vec<(String, String, u8)> = tree
            .get_files()
            .chain(tree.get_subtrees())
            .map(|(name, item)| (name.clone(), item.hash.clone(), dir_code(item.item_type)))
            .collect();
        let mut got: Vec<(String, String, u8)> = parsed
            .get_files()
            .chain(parsed.get_subtrees())
            .map(|(name, item)| (name.clone(), item.hash.clone(), dir_code(item.item_type)))
            .collect();
        want.sort();
        got.sort();
        assert_eq!(got, want);
    }
}

#[test]
fn parcels_round_trip() {
    let mut rng = Rng::new(0x9A2C_E104);
    for _ in 0..500 {
        let parcel = parcel_seed(&mut rng);
        let bytes = LooseObjectBuilder::build_parcel(&parcel).content;
        let parsed = match parse_loose_object(&bytes).unwrap() {
            ParsedObject::Parcel(p) => p,
            other => panic!("a parcel parsed as {:?}", std::mem::discriminant(&other)),
        };

        assert_eq!(parsed.tree_hash, parcel.tree_hash);
        assert_eq!(parsed.parents, parcel.parents);
        assert_eq!(parsed.description, parcel.description);
        assert_eq!(parsed.actions.len(), parcel.actions.len());
        for (got, want) in parsed.actions.iter().zip(parcel.actions.iter()) {
            assert_eq!(got.operator.identifier, want.operator.identifier);
            assert_eq!(got.action.get_code(), want.action.get_code());
            assert_eq!(got.description, want.description);
            assert_eq!(got.timestamp, want.timestamp);
        }
    }
}

#[test]
fn inventories_round_trip() {
    let mut rng = Rng::new(0x147E_0005);
    for _ in 0..500 {
        let (inventory, names) = inventory_seed(&mut rng);
        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        assert_eq!(parsed.get_items_count(), names.len());
        for name in &names {
            let want = inventory.get_item_by_name(name).unwrap();
            let got = parsed.get_item_by_name(name).unwrap();
            assert_eq!(got.hash, want.hash);
            assert_eq!(got.file_size, want.file_size);
            assert_eq!(got.inode, want.inode);
            assert_eq!(got.device, want.device);
            assert!(got.state == want.state, "state mismatch for {name:?}");
            assert_eq!(dir_code(got.item_type), dir_code(want.item_type));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Regressions — the exact crash inputs this suite first surfaced, pinned by name.
// ---------------------------------------------------------------------------------------------

mod regressions {
    use super::*;

    /// A tree entry whose length prefix is `u64::MAX`: the parser used to compute `start + length`
    /// before the bounds check (`attempt to add with overflow` in debug; a wrapped, out-of-range
    /// slice in release). Now reported as truncated.
    #[test]
    fn tree_entry_with_a_huge_name_length_is_rejected_not_panicked() {
        let mut body = Vec::new();
        body.extend(number_to_vlq_bytes(2)); // latest tree body version
        body.extend(number_to_vlq_bytes(1)); // entry type: Normal file
        body.extend(number_to_vlq_bytes(u64::MAX)); // name length
        body.push(b'x');
        let err = forklift_core::parser::object::tree::tree_parser::parse_tree(0, &body);
        assert!(err.is_err(), "a huge name length must be an error, not a panic");
    }

    /// The inventory analog of the tree case (its own parser, same anti-pattern).
    #[test]
    fn inventory_item_with_a_huge_name_length_is_rejected_not_panicked() {
        let mut bytes = Vec::new();
        bytes.extend(number_to_vlq_bytes(1)); // inventory version
        bytes.extend(number_to_vlq_bytes(1)); // entry count (discarded)
        bytes.push(0x00); // end of header
        for _ in 0..8 {
            bytes.extend(number_to_vlq_bytes(1)); // the 8 leading VLQ fields
        }
        bytes.extend(b"abcd");
        bytes.push(0x03); // end-of-text after the hash
        bytes.extend(number_to_vlq_bytes(u64::MAX)); // name length
        bytes.extend(number_to_vlq_bytes(1)); // state
        bytes.push(b'x');
        assert!(parse_inventory(&bytes).is_err());
    }

    /// The parcel analog: `read_length_prefixed_string` (operator id and action description) had
    /// the same `start + length` overflow. A parcel with a valid header but a huge action id
    /// length must error.
    #[test]
    fn parcel_with_a_huge_field_length_is_rejected_not_panicked() {
        let mut body = Vec::new();
        body.extend(number_to_vlq_bytes(2)); // latest parcel body version
        body.extend(SAMPLE_HASH.as_bytes());
        body.push(b'\n'); // tree hash line
        body.push(0x00); // no parents
        body.extend(number_to_vlq_bytes(1)); // action type: Author
        body.extend(number_to_vlq_bytes(1_700_000_000)); // timestamp
        body.extend(number_to_vlq_bytes(u64::MAX)); // operator-id length
        body.push(b'x');
        let err = forklift_core::parser::object::parcel::compact_parcel_parser::parse_compact_parcel(0, &body);
        assert!(err.is_err(), "a huge field length must be an error, not a panic");
    }

    /// A bundle can arrive from an untrusted remote (`franchise` downloads one), and its records
    /// carry an 8-byte length prefix. A record that declares a `u64::MAX` length used to
    /// `vec![0u8; length]` before reading — a one-record denial of service (capacity-overflow
    /// panic / allocator abort). Import now reads the record as a bounded stream and reports the
    /// short stream as truncation. This is the most exposed of the length bugs, so it is pinned
    /// end-to-end through `import_bundle_bytes`.
    #[test]
    fn bundle_import_rejects_a_huge_record_length_without_allocating_it() {
        // A private storage scope: `import` resolves the object root from the thread-local scope,
        // and the scope is thread-local so this stays isolated from the parallel tests.
        let temp = std::env::temp_dir().join(format!("forklift-fuzz-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = forklift_core::globals::StorageRootScope::enter(&temp);

        let mut raw = Vec::new();
        raw.extend(b"forklift-bundle 2026-07-06");
        raw.push(b'\n');
        let mut record = Vec::new();
        record.push(b'O'); // an object record
        record.extend([b'a'; 64]); // its (ASCII) hash
        record.extend(u64::MAX.to_be_bytes()); // a hostile declared length
        record.extend(b"only a few real bytes");
        raw.extend(zstd::encode_all(&record[..], 3).unwrap());

        let result = forklift_core::util::bundle_utils::import_bundle_bytes(&raw);
        assert!(result.is_err(), "a huge record length must be an error, not a panic/abort");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A delta record's declared decompressed length is attacker-controlled. It must neither be
    /// pre-allocated (a capacity-overflow panic / allocator abort) nor trusted as the output
    /// bound — a declared `u64::MAX` would be no bound at all, and a small zstd frame could then
    /// expand until memory ran out.
    #[test]
    fn a_delta_with_a_dishonest_declared_length_is_refused_without_allocating_it() {
        use forklift_core::util::delta_utils::{
            compress_delta, decompress_delta, MAX_DELTA_TARGET_BYTES,
        };

        let base = b"the quick brown fox\n".repeat(8);
        let mut target = base.clone();
        target.extend_from_slice(b"one more line\n");
        let delta = compress_delta(&base, &target).unwrap();

        // A truthful capacity still reconstructs, exactly.
        assert_eq!(decompress_delta(&base, &delta, target.len()).unwrap(), target);

        // An enormous declared length is refused up front, by the ceiling — never allocated,
        // never used as the read bound. This is the decompression-bomb guard.
        let error = decompress_delta(&base, &delta, u64::MAX as usize)
            .expect_err("a u64::MAX target must be refused by the ceiling");
        assert!(error.contains("ceiling"), "{}", error);

        let error = decompress_delta(&base, &delta, MAX_DELTA_TARGET_BYTES + 1)
            .expect_err("one byte over the ceiling is still over it");
        assert!(error.contains("ceiling"), "{}", error);

        // A lie that stays *under* the ceiling is caught by the exact-length check instead, and
        // the 16 MiB it declared is never allocated — the buffer grows with the bytes produced.
        let error = decompress_delta(&base, &delta, MAX_DELTA_TARGET_BYTES)
            .expect_err("the frame does not reconstruct to the declared length");
        assert!(error.contains("declared"), "{}", error);

        // And a frame that overruns a small declared length is rejected, not truncated.
        let error = decompress_delta(&base, &delta, 4).expect_err("an overrunning frame");
        assert!(error.contains("declared"), "{}", error);
    }
}
