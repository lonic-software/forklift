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
use forklift_core::enums::object::compact_parcel_version::CompactParcelVersion;
use forklift_core::enums::object::loose_object_version::LooseObjectVersion;
use forklift_core::enums::object::recipe_version::RecipeVersion;
use forklift_core::enums::object::tree_version::TreeVersion;
use forklift_core::enums::object_type::ObjectType;
use forklift_core::model::chunk::Chunk;
use forklift_core::model::recipe::{Recipe, RecipeChunk};
use forklift_core::util::byte_utils::{number_from_vlq_bytes, number_to_vlq_bytes};
use forklift_core::util::chunk_utils::MAX_CHUNK_BYTES;
use forklift_core::util::meta_tree::{encode_cell, parse_cell, Stamp};
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::query_utils::{
    parse_where, MAX_GLOB_CHARS, MAX_IN_VALUES, MAX_PREDICATE_DEPTH, MAX_PREDICATE_LEAVES, MAX_WHERE_BYTES,
};
use serde_json::{json, Value};

#[path = "../src/util/peak_alloc.rs"]
mod peak_alloc;

#[global_allocator]
static PEAK_ALLOC: peak_alloc::PeakAlloc = peak_alloc::PeakAlloc;

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
        DirEntryType::NormalChunked => 5,
        DirEntryType::ExecutableChunked => 6,
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
    blob_object_bytes_up_to(rng, 64)
}

fn blob_object_bytes_up_to(rng: &mut Rng, max_len: usize) -> Vec<u8> {
    let len = rng.below(max_len);
    let content = (0..len).map(|_| rng.byte()).collect();
    LooseObjectBuilder::build_blob(&Blob { content }).content
}

/// A tree with unique child names (the builder keys children by name, so duplicates collapse and
/// would break a naive round-trip count).
fn tree_seed(rng: &mut Rng) -> TreeItem {
    tree_seed_up_to(rng, 6)
}

fn tree_seed_up_to(rng: &mut Rng, max_entries: usize) -> TreeItem {
    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut names = std::collections::HashSet::new();
    for _ in 0..rng.below(max_entries) {
        // A tree entry is one path component, so the parser refuses a `/` the pool carries for
        // inventory paths.
        let name = random_name(rng).replace('/', ".");
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
    inventory_seed_up_to(rng, 6)
}

fn inventory_seed_up_to(rng: &mut Rng, max_items: usize) -> (Inventory, Vec<String>) {
    let mut inventory = Inventory::new();
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..rng.below(max_items) {
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

/// A meta-tree cell as `encode_cell` writes it: a signer or operator stamp, a schema, and a value
/// of strings, integers, booleans, arrays and a nested table.
fn cell_bytes(rng: &mut Rng) -> Vec<u8> {
    cell_bytes_up_to(rng, 5)
}

fn cell_bytes_up_to(rng: &mut Rng, max_keys: usize) -> Vec<u8> {
    let stamp = if rng.bool() { Stamp::Signer(random_hash(rng)) } else { Stamp::Operator(random_name(rng)) };
    let mut value = toml_edit::Table::new();

    for _ in 0..rng.below(max_keys) {
        let item = match rng.below(5) {
            0 => toml_edit::value(random_name(rng)),
            1 => toml_edit::value(rng.next_u64() as i64),
            2 => toml_edit::value(rng.bool()),
            3 => toml_edit::value(toml_edit::Array::from_iter((0..rng.below(3)).map(|_| random_name(rng)))),
            _ => {
                let mut nested = toml_edit::Table::new();
                nested.insert(&random_name(rng), toml_edit::value(random_name(rng)));
                toml_edit::Item::Table(nested)
            }
        };
        value.insert(&random_name(rng), item);
    }

    encode_cell(&stamp, "fuzz.shape 1", &value).expect("the generator writes only kinds a cell holds")
}

// ---------------------------------------------------------------------------------------------
// The never-panic property.
// ---------------------------------------------------------------------------------------------

/// A named parse entry point: a label and a function that parses (discarding the result — a parse
/// error is fine here, only a panic is the bug).
type ParseEntryPoint = (&'static str, fn(&[u8]));

/// Every public parse entry point for bytes read from storage or a remote.
const ENTRY_POINTS: [ParseEntryPoint; 3] = [
    ("loose_object", |b| {
        let _ = parse_loose_object(b);
    }),
    ("inventory", |b| {
        let _ = parse_inventory(b);
    }),
    ("meta_cell", |b| {
        let _ = parse_cell(b);
    }),
];

/// Feed one byte string to every parser under `catch_unwind`. A parse result of `Ok` or `Err` is
/// equally acceptable; a panic is the bug. Returns the offending description on panic so the
/// caller can print the reproducing bytes.
fn parse_never_panics(bytes: &[u8]) -> Result<(), String> {
    for (label, parse) in ENTRY_POINTS {
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

    let generators: [fn(&mut Rng) -> Vec<u8>; 5] =
        [blob_object_bytes, tree_object_bytes, parcel_object_bytes, inventory_bytes, cell_bytes];

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

    assert!(cases >= 24_000, "the fuzzer should exercise a large corpus, ran {cases}");
}

// ---------------------------------------------------------------------------------------------
// The allocation bound: a decoder holds memory in proportion to the bytes it was given.
// ---------------------------------------------------------------------------------------------

/// Each entry point's bound: at its peak, a parse of `n` bytes holds at most `factor * n +
/// overhead` bytes, counted by the test allocator (`peak_alloc`) on the calling thread, the
/// parsed value included. The factor is what a decoder needs to copy its input into a parsed
/// value; the overhead is the fixed cost of a parse. A decoder that sized an allocation from a
/// number in its input, rather than from the bytes behind it, would exceed the overhead on the
/// first such input.
///
/// The factors are measured ones with headroom, not derived limits:
/// * a loose object: a blob holds about its bytes once, a tree about 7.4 times (each parsed entry
///   owns its name twice, as the entry's and as its map key, and its hash as 64 hex characters);
/// * an inventory: about 2.2 times;
/// * a meta cell: about 55 times on a large cell, since every value becomes a `serde_json::Value`
///   whose nodes outweigh the bytes that spell them. A cell's size is capped only by the blob
///   ceiling, so this factor is the one a hostile cell can multiply.
const ENTRY_POINT_BOUNDS: [(&str, usize, usize); 3] = [
    ("loose_object", 10, 16 * 1024),
    ("inventory", 3, 16 * 1024),
    ("meta_cell", 64, 64 * 1024),
];

// A row missing from `ENTRY_POINT_BOUNDS` would be dropped by the `zip` below, unbounded.
const _: () = assert!(ENTRY_POINT_BOUNDS.len() == ENTRY_POINTS.len());

/// Measure every entry point on `bytes` and check it against its bound.
fn parse_within_bounds(bytes: &[u8], case: &str) {
    for ((label, parse), (bound_label, factor, overhead)) in ENTRY_POINTS.iter().zip(ENTRY_POINT_BOUNDS) {
        assert_eq!(*label, bound_label, "ENTRY_POINT_BOUNDS lists the entry points in ENTRY_POINTS order");
        let ((), peak) = peak_alloc::measure(|| parse(bytes));
        peak_alloc::assert_within(label, case, bytes.len(), peak, factor, overhead);
    }
}

#[test]
fn every_parser_holds_memory_in_proportion_to_its_input() {
    let mut rng = Rng::new(0xF01C_11F7_5EED_0427);

    // Small objects, mutated: these reach the length prefixes and counts a hostile input sets.
    let generators: [fn(&mut Rng) -> Vec<u8>; 7] = [
        blob_object_bytes,
        tree_object_bytes,
        parcel_object_bytes,
        |rng| LooseObjectBuilder::build_recipe(&recipe_seed(rng)).content,
        |rng| LooseObjectBuilder::build_chunk(&Chunk { content: (0..rng.below(64)).map(|_| rng.byte()).collect() }).content,
        inventory_bytes,
        cell_bytes,
    ];
    for case in 0..2000 {
        for generate in generators {
            let mut bytes = generate(&mut rng);
            mutate(&mut bytes, &mut rng);
            parse_within_bounds(&bytes, &format!("mutated case {case}"));
        }
    }

    // Large objects, whole and mutated: these are where a per-byte factor shows.
    let large: [fn(&mut Rng) -> Vec<u8>; 6] = [
        |rng| blob_object_bytes_up_to(rng, 256 * 1024),
        |rng| LooseObjectBuilder::build_tree(&tree_seed_up_to(rng, 2000)).content,
        |rng| {
            let mut parcel = parcel_seed(rng);
            parcel.parents = (0..rng.below(2000)).map(|_| random_hash(rng)).collect();
            LooseObjectBuilder::build_parcel(&parcel).content
        },
        |rng| LooseObjectBuilder::build_recipe(&recipe_seed_up_to(rng, 2000)).content,
        |rng| InventoryBuilder::build(&inventory_seed_up_to(rng, 2000).0),
        |rng| cell_bytes_up_to(rng, 2000),
    ];
    for case in 0..30 {
        for generate in large {
            let mut bytes = generate(&mut rng);
            parse_within_bounds(&bytes, &format!("large case {case}"));
            mutate(&mut bytes, &mut rng);
            parse_within_bounds(&bytes, &format!("large mutated case {case}"));
        }
    }

    // Random bytes.
    for case in 0..2000 {
        let bytes: Vec<u8> = (0..rng.below(256)).map(|_| rng.byte()).collect();
        parse_within_bounds(&bytes, &format!("random case {case}"));
    }
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

/// A written cell parses back to the same envelope, and re-encodes to the same bytes.
#[test]
fn cells_round_trip() {
    let mut rng = Rng::new(0xCE11_2002);
    for _ in 0..500 {
        let bytes = cell_bytes(&mut rng);
        let parsed = parse_cell(&bytes).unwrap_or_else(|e| panic!("{}: {:?}", e, String::from_utf8_lossy(&bytes)));
        assert_eq!(encode_cell(&parsed.stamp, &parsed.schema, &parsed.value).unwrap(), bytes);
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

    /// A batch or subtree response arrives from an untrusted remote, and its records carry an
    /// 8-byte length prefix. A record that declares a `u64::MAX` length used to
    /// `vec![0u8; length]` before reading — a one-record denial of service (capacity-overflow
    /// panic / allocator abort). This is the most exposed of the length bugs, so it is pinned
    /// end-to-end through `import_streamed_bundle`, the public path those responses take, with
    /// the record's hash requested so the length itself is what gets judged.
    #[test]
    fn bundle_import_rejects_a_huge_record_length_without_allocating_it() {
        // A private storage scope: `import` resolves the object root from the thread-local scope,
        // and the scope is thread-local so this stays isolated from the parallel tests.
        let temp = std::env::temp_dir().join(format!("forklift-fuzz-bundle-{}", std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = forklift_core::globals::StorageRootScope::enter(&temp);

        let mut raw = Vec::new();
        raw.extend(b"forklift-bundle 2026-07-11");
        raw.push(b'\n');
        let mut record = Vec::new();
        record.push(b'O'); // an object record
        record.extend([b'a'; 64]); // its (ASCII) hash
        record.extend(u64::MAX.to_be_bytes()); // a hostile declared length
        record.extend(b"only a few real bytes");
        raw.extend(zstd::encode_all(&record[..], 3).unwrap());

        use forklift_core::util::bundle_utils::{import_streamed_bundle, StreamAdmission};
        let admission = StreamAdmission::Requested(std::collections::HashSet::from(["a".repeat(64)]));
        let result = import_streamed_bundle(std::io::Cursor::new(&raw), admission);
        let error = result.err().expect("a huge record length must be an error, not a panic/abort");
        assert!(error.contains("declares 18446744073709551615 bytes"), "refused on its length: {}", error);

        forklift_core::util::file_utils::remove_dir_all(&temp).ok();
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

// ---------------------------------------------------------------------------------------------
// The canonical property: one value, one spelling.
// ---------------------------------------------------------------------------------------------

/// A loose object around `body`: the latest header version, the type, the body length, a null.
fn wrap(object_type: ObjectType, body: &[u8]) -> Vec<u8> {
    let mut bytes = number_to_vlq_bytes(LooseObjectVersion::latest().get_code());
    bytes.extend(number_to_vlq_bytes(object_type.get_code()));
    bytes.extend(number_to_vlq_bytes(body.len() as u64));
    bytes.push(0);
    bytes.extend_from_slice(body);
    bytes
}

fn recipe_seed(rng: &mut Rng) -> Recipe {
    recipe_seed_up_to(rng, 5)
}

fn recipe_seed_up_to(rng: &mut Rng, max_chunks: usize) -> Recipe {
    let chunks: Vec<RecipeChunk> = (0..rng.below(max_chunks))
        .map(|_| RecipeChunk { hash: random_hash(rng), size: 1 + rng.below(MAX_CHUNK_BYTES) as u64 })
        .collect();
    Recipe {
        content_hash: random_hash(rng),
        total_size: chunks.iter().map(|chunk| chunk.size).sum(),
        chunks,
    }
}

/// `text` without the bytes the first tree and parcel versions use as terminators (null, end of
/// text, new line), which those versions cannot hold inside a name or a description; that is why
/// the second versions length-prefix them.
fn first_version_text(text: &str) -> String {
    text.replace(['\u{0}', '\u{3}', '\n'], ".")
}

/// One valid object of every type in every body version this build reads, each written by that
/// version's own builder (not by `LooseObjectBuilder`, which writes only the latest versions).
fn every_version_fixtures(rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    let mut fixtures = vec![
        ("blob".to_string(), blob_object_bytes(rng)),
        ("chunk".to_string(), LooseObjectBuilder::build_chunk(&Chunk { content: (0..rng.below(64)).map(|_| rng.byte()).collect() }).content),
    ];
    for code in 1.. {
        let Ok(version) = TreeVersion::from_code(code) else { break };
        let mut tree = tree_seed(rng);
        if code == 1 {
            let seed = tree;
            tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            for (name, child) in seed.get_subtrees().chain(seed.get_files()) {
                tree.add_child(TreeItem::new(first_version_text(name), child.hash.clone(), child.item_type));
            }
        }
        fixtures.push((format!("tree v{}", code), wrap(ObjectType::Tree, &version.get_builder()(&tree))));
    }
    for code in 1.. {
        let Ok(version) = CompactParcelVersion::from_code(code) else { break };
        let mut parcel = parcel_seed(rng);
        if code == 1 {
            for action in &mut parcel.actions {
                action.description = action.description.as_deref().map(first_version_text);
            }
            parcel.description = parcel.description.as_deref().map(first_version_text);
        }
        fixtures.push((format!("parcel v{}", code), wrap(ObjectType::Parcel, &version.get_builder()(&parcel))));
    }
    for code in 1.. {
        let Ok(version) = RecipeVersion::from_code(code) else { break };
        fixtures.push((format!("recipe v{}", code), wrap(ObjectType::Recipe, &version.get_builder()(&recipe_seed(rng)))));
    }
    fixtures
}

/// The bytes the format writes for the object `bytes` parsed to, in the body version `bytes`
/// declares.
fn rebuild(bytes: &[u8], parsed: &ParsedObject) -> Vec<u8> {
    let mut at = 0;
    for _ in 0..3 {
        at += number_from_vlq_bytes(at, bytes).unwrap().1;
    }
    at += 1;
    let body_version = || number_from_vlq_bytes(at, bytes).unwrap().0;

    match parsed {
        ParsedObject::Blob(blob) => LooseObjectBuilder::build_blob(blob).content,
        ParsedObject::Chunk(chunk) => LooseObjectBuilder::build_chunk(chunk).content,
        ParsedObject::Tree(tree) =>
            wrap(ObjectType::Tree, &TreeVersion::from_code(body_version()).unwrap().get_builder()(tree)),
        ParsedObject::Parcel(parcel) =>
            wrap(ObjectType::Parcel, &CompactParcelVersion::from_code(body_version()).unwrap().get_builder()(parcel)),
        ParsedObject::Recipe(recipe) =>
            wrap(ObjectType::Recipe, &RecipeVersion::from_code(body_version()).unwrap().get_builder()(recipe)),
    }
}

/// Rewrite one byte below `0x80` of a whole object as the two-byte overlong spelling of the same
/// value (`b | 0x80`, then `00`), fixing up the declared content length when the byte lies in the
/// body. Where the byte ends a VLQ this is a second spelling of the same number; random mutation
/// almost never produces one with the length still consistent.
fn respell_a_vlq_end(bytes: &mut Vec<u8>, rng: &mut Rng) {
    let mut header_len = 0;
    for _ in 0..3 {
        header_len += number_from_vlq_bytes(header_len, bytes).unwrap().1;
    }
    header_len += 1;
    let object_type = ObjectType::from_code(number_from_vlq_bytes(number_from_vlq_bytes(0, bytes).unwrap().1, bytes).unwrap().0).unwrap();

    let candidates: Vec<usize> = (0..bytes.len()).filter(|&at| at + 1 != header_len && bytes[at] < 0x80).collect();
    if candidates.is_empty() {
        return;
    }
    let at = candidates[rng.below(candidates.len())];
    let respelled = [bytes[at] | 0x80, 0x00];
    bytes.splice(at..at + 1, respelled);
    if at >= header_len {
        *bytes = wrap(object_type, &bytes[header_len..].to_vec());
    }
}

/// Any bytes the loose-object parser accepts are exactly the bytes the format writes for what they
/// parsed to: an object is named by the hash of its bytes, so a second spelling of one value
/// would give it a second name, out of reach of a signature or a redaction naming the first.
///
/// The fixtures cover every body version, and the rebuild uses the version the bytes declare, so
/// this also pins the builders of the older versions: they are part of the format, and changing
/// what one writes would leave every object it already wrote unreadable.
///
/// Revert: drop the shortest-VLQ rule, the header's null or length check, or the tree or parcel
/// canonical comparison → mutated inputs parse to a value that rebuilds differently. The first
/// recipe version is canonical by construction once its numbers are (fixed-length hashes, no
/// trailing bytes), so its comparison is reached only with the shortest-VLQ rule dropped too.
#[test]
fn every_accepted_object_is_in_its_canonical_form() {
    let mut rng = Rng::new(0xC0DE_0001);
    let mut accepted: std::collections::BTreeMap<String, usize> = Default::default();

    for _ in 0..3000 {
        for (label, fixture) in every_version_fixtures(&mut rng) {
            let parsed = parse_loose_object(&fixture).unwrap_or_else(|e| panic!("{}: {}", label, e));
            assert_eq!(rebuild(&fixture, &parsed), fixture, "{} does not round-trip", label);

            let mut bytes = fixture;
            if rng.below(3) == 0 {
                respell_a_vlq_end(&mut bytes, &mut rng);
            } else {
                mutate(&mut bytes, &mut rng);
            }
            let Ok(parsed) = parse_loose_object(&bytes) else { continue };
            let rebuilt = rebuild(&bytes, &parsed);
            assert!(rebuilt == bytes, "{} mutated to a second spelling\n  in : {:02x?}\n  out: {:02x?}",
                label, bytes, rebuilt);
            *accepted.entry(label).or_default() += 1;
        }
    }

    // The property is only as strong as the mutated inputs that still parse.
    for label in ["blob", "chunk", "tree v1", "tree v2", "parcel v1", "parcel v2", "recipe v1"] {
        assert!(accepted.get(label).copied().unwrap_or(0) >= 20, "too few accepted {} mutations: {:?}", label, accepted);
    }
}

// ---------------------------------------------------------------------------------------------
// Query predicates: the `--where` JSON a client (or an agent, through the MCP server) hands the
// query engine. Three properties: no input panics the decoder; every refusal is the one
// predicate refusal; and a predicate is accepted exactly when the grammar below accepts it, so
// a malformed or out-of-bounds one is refused, never read as something wider.
// ---------------------------------------------------------------------------------------------

const CLASSES: [&str; 4] = ["human", "agent", "bot", "service"];
const ROLES: [&str; 3] = ["admin", "writer", "reader"];
const DATE: &str = "2026-03-01T12:00:00Z";

/// The predicate grammar as the query guide states it, written without the decoder: whether
/// `node`, at `depth` (the root is 1), is a predicate, counting leaves into `leaves`.
fn grammar_accepts(node: &Value, depth: usize, leaves: &mut usize) -> bool {
    if depth > MAX_PREDICATE_DEPTH {
        return false;
    }
    let Some(object) = node.as_object() else { return false };
    let combinators: Vec<&String> = object.keys().filter(|key| ["all", "any", "not"].contains(&key.as_str())).collect();
    match combinators.as_slice() {
        [] => {
            *leaves += 1;
            *leaves <= MAX_PREDICATE_LEAVES && grammar_accepts_leaf(object)
        }
        [combinator] if object.len() == 1 => match (combinator.as_str(), &object[combinator.as_str()]) {
            ("not", inner) => grammar_accepts(inner, depth + 1, leaves),
            (_, Value::Array(children)) => children.iter().all(|child| grammar_accepts(child, depth + 1, leaves)),
            _ => false,
        },
        _ => false,
    }
}

fn grammar_accepts_leaf(object: &serde_json::Map<String, Value>) -> bool {
    if object.keys().any(|key| !["field", "op", "value"].contains(&key.as_str())) {
        return false;
    }
    let (Some(field), Some(op), Some(value)) =
        (object.get("field").and_then(Value::as_str), object.get("op").and_then(Value::as_str), object.get("value"))
    else {
        return false;
    };

    // eq/ne take one value, in an array of at most MAX_IN_VALUES; `ne_ok` says whether ne applies.
    let membership = |ne_ok: bool, each: &dyn Fn(&Value) -> bool| match op {
        "eq" => each(value),
        "ne" => ne_ok && each(value),
        "in" => value.as_array().is_some_and(|items| items.len() <= MAX_IN_VALUES && items.iter().all(each)),
        _ => false,
    };
    let string = |value: &Value| value.is_string();
    let one_of = |names: &'static [&'static str]| move |value: &Value| value.as_str().is_some_and(|text| names.contains(&text));
    let hash_prefix = |value: &Value| {
        value.as_str().is_some_and(|text| {
            (1..=64).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    };
    let date = |value: &Value| value.as_str().is_some_and(|text| chrono::DateTime::parse_from_rfc3339(text).is_ok());
    let glob = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty() && text.chars().count() <= MAX_GLOB_CHARS);

    match field {
        "author.operator" | "stacker.operator" | "signer.operator" => membership(true, &string),
        "author.date" | "stacker.date" => match op {
            "before" | "after" => date(value),
            "between" => value.as_array().is_some_and(|bounds| bounds.len() == 2 && bounds.iter().all(date)),
            _ => false,
        },
        "author.class" | "signer.class" => membership(true, &one_of(&CLASSES)),
        "author.supervisor" => membership(true, &|value: &Value| value.is_string() || value.is_null()),
        "author.role" => membership(true, &one_of(&ROLES)),
        "signer.key" | "parcel" => membership(false, &hash_prefix),
        "signer.boundary" => membership(false, &one_of(&["vouched", "suspect"])),
        "description" => op == "matches" && glob(value),
        "is_merge" => op == "eq" && value.is_boolean(),
        "parents.count" => op == "eq" && value.as_u64().is_some(),
        "provenance.model" | "provenance.tool" | "provenance.session" => {
            if op == "matches" { glob(value) } else { membership(true, &string) }
        }
        "tag" => membership(false, &string),
        "path" => {
            op == "touches"
                && value.as_str().is_some_and(warehouse_relative)
        }
        _ => false,
    }
}

/// A path the grammar admits, decided without the decoder's own path reader: non-empty, free of
/// control characters, relative, and never climbing above the root. (The test process stands in
/// no warehouse, so a relative path is read from the root.)
fn warehouse_relative(text: &str) -> bool {
    if text.is_empty() || text.chars().any(char::is_control) || text.starts_with('/') {
        return false;
    }
    let mut depth = 0usize;
    for segment in text.split('/') {
        match segment {
            "" | "." => {}
            ".." => match depth.checked_sub(1) {
                Some(up) => depth = up,
                None => return false,
            },
            _ => depth += 1,
        }
    }
    true
}

/// A valid leaf for every field the grammar names.
fn predicate_leaf(rng: &mut Rng) -> Value {
    let pick = |rng: &mut Rng, names: &[&str]| names[rng.below(names.len())].to_string();
    let leaf = |field: &str, op: &str, value: Value| json!({ "field": field, "op": op, "value": value });
    match rng.below(14) {
        0 => leaf(&pick(rng, &["author.operator", "stacker.operator", "signer.operator"]), &pick(rng, &["eq", "ne"]), json!(random_name(rng))),
        1 => leaf(&pick(rng, &["author.date", "stacker.date"]), &pick(rng, &["before", "after"]), json!(DATE)),
        2 => leaf("author.date", "between", json!([DATE, "2026-04-01T00:00:00+02:00"])),
        3 => leaf(&pick(rng, &["author.class", "signer.class"]), "in", json!([pick(rng, &CLASSES), pick(rng, &CLASSES)])),
        4 => leaf("author.supervisor", "eq", if rng.bool() { Value::Null } else { json!("alice@corp") }),
        5 => leaf("author.role", "ne", json!(pick(rng, &ROLES))),
        6 => leaf(&pick(rng, &["signer.key", "parcel"]), "eq", json!(&random_hash(rng)[..1 + rng.below(64)])),
        7 => leaf("signer.boundary", "in", json!(["vouched", "suspect"])),
        8 => leaf("description", "matches", json!(pick(rng, &["fix", "fix*walk", "?", "*"]))),
        9 => leaf("is_merge", "eq", json!(rng.bool())),
        10 => leaf("parents.count", "eq", json!(rng.below(3))),
        11 => leaf(&pick(rng, &["provenance.model", "provenance.tool", "provenance.session"]), &pick(rng, &["matches", "eq"]), json!("claude-*")),
        12 => leaf("tag", "in", json!(["v1.0", random_name(rng)])),
        _ => leaf("path", "touches", json!(pick(rng, &["src", "src/app.rs", "."]))),
    }
}

/// A valid predicate tree, at most `room` levels deep.
fn predicate_tree(rng: &mut Rng, room: usize) -> Value {
    if room <= 1 || rng.below(3) == 0 {
        return predicate_leaf(rng);
    }
    match rng.below(3) {
        0 => json!({ "not": predicate_tree(rng, room - 1) }),
        kind => {
            let children: Vec<Value> = (0..rng.below(4)).map(|_| predicate_tree(rng, room - 1)).collect();
            if kind == 1 { json!({ "all": children }) } else { json!({ "any": children }) }
        }
    }
}

/// Values a mutation writes into a tree: each sits at the edge of some rule.
fn edge_value(rng: &mut Rng) -> Value {
    let strings = [
        "", "*", "fix*", "9f", "9F", "9g", "ABC", "human", "agent", "admin", "vouched", "suspect", "unresolved",
        DATE, "2026-13-01T00:00:00Z", "yesterday", "all", "any", "not", "eq", "ne", "in", "matches", "touches",
        "between", "field", "op", "value", "src", ".", "../outside", "src/..", "src/../..", "./", "a/./b", "/abs", "a\u{0}b", "\u{e9}",
    ];
    match rng.below(12) {
        0 => Value::Null,
        1 => json!(rng.bool()),
        2 => json!([0i64, -1, 1, 2, i64::MIN][rng.below(5)]),
        3 => json!(u64::MAX),
        4 => json!(1.5),
        5 => json!([]),
        6 => json!({}),
        7 => json!([DATE, DATE]),
        8 => Value::Array((0..MAX_IN_VALUES + rng.below(2)).map(|i| json!(format!("v{i}"))).collect()),
        9 => json!("f".repeat(64 + rng.below(2))),
        10 => json!("x".repeat(MAX_GLOB_CHARS + rng.below(2))),
        _ => json!(strings[rng.below(strings.len())]),
    }
}

/// Path spellings for a `touches` leaf: some name a place inside the warehouse, the root among
/// them, and the rest climb out of it, are absolute, empty or carry a control character.
const PATH_SPELLINGS: &[&str] = &[
    "src", "src/app.rs", ".", "./", "src/..", "a/./b", "a//b", "src/", "\u{e9}/x",
    "", "..", "../outside", "src/../..", "a/../../b", "/abs", "/", "a\u{1}b", "a\tb",
];

/// Give every `touches` leaf in `node` a spelling from [`PATH_SPELLINGS`].
fn respell_paths(node: &mut Value, rng: &mut Rng) {
    match node {
        Value::Object(object) => {
            if object.get("op") == Some(&json!("touches")) && object.contains_key("value") {
                object.insert("value".to_string(), json!(PATH_SPELLINGS[rng.below(PATH_SPELLINGS.len())]));
            }
            object.values_mut().for_each(|child| respell_paths(child, rng));
        }
        Value::Array(items) => items.iter_mut().for_each(|child| respell_paths(child, rng)),
        _ => {}
    }
}

/// Every value position in `node`, as a path of object keys and array indexes.
fn value_paths(node: &Value, here: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    out.push(here.clone());
    match node {
        Value::Object(object) => object.iter().for_each(|(key, child)| {
            here.push(key.clone());
            value_paths(child, here, out);
            here.pop();
        }),
        Value::Array(items) => items.iter().enumerate().for_each(|(index, child)| {
            here.push(index.to_string());
            value_paths(child, here, out);
            here.pop();
        }),
        _ => {}
    }
}

fn at_path<'a>(node: &'a mut Value, path: &[String]) -> &'a mut Value {
    path.iter().fold(node, |node, step| match node {
        Value::Object(object) => object.get_mut(step).unwrap(),
        Value::Array(items) => &mut items[step.parse::<usize>().unwrap()],
        _ => unreachable!("a path only steps into containers"),
    })
}

/// One structural mutation: replace a value, rename, add or drop a key, or bury a node under
/// enough `not`s to cross the depth bound, or under enough siblings to cross the leaf bound.
fn mutate_predicate(tree: &mut Value, rng: &mut Rng) {
    let mut paths = Vec::new();
    value_paths(tree, &mut Vec::new(), &mut paths);
    let path = paths[rng.below(paths.len())].clone();
    let keys = ["field", "op", "value", "all", "any", "not", "values"];
    let target = at_path(tree, &path);
    match rng.below(6) {
        0 | 1 => *target = edge_value(rng),
        2 => {
            if let Value::Object(object) = target {
                let key = keys[rng.below(keys.len())].to_string();
                object.insert(key, edge_value(rng));
            }
        }
        3 => {
            if let Value::Object(object) = target {
                if let Some(key) = object.keys().nth(rng.below(object.len().max(1))).cloned() {
                    let value = object.remove(&key).unwrap();
                    object.insert(keys[rng.below(keys.len())].to_string(), value);
                }
            }
        }
        4 => {
            for _ in 0..MAX_PREDICATE_DEPTH - 2 + rng.below(4) {
                *target = json!({ "not": target.take() });
            }
        }
        _ => {
            let siblings = MAX_PREDICATE_LEAVES - 2 + rng.below(4);
            *target = json!({ "all": vec![target.take(); siblings] });
        }
    }
}

/// The predicate parser's bound, as `ENTRY_POINT_BOUNDS` states its: measured up to about 96 times
/// its input (a `serde_json::Value` tree, then the predicate built from it), on inputs capped at
/// `MAX_WHERE_BYTES`, so at most about 8 MiB.
const PREDICATE_FACTOR: usize = 128;
const PREDICATE_OVERHEAD: usize = 16 * 1024;

/// The verdict for one payload, under `catch_unwind`, with its memory checked against the
/// predicate parser's bound: `Ok(accepted)`, or a description of the
/// panic or of an error that is not the predicate refusal.
fn decode_predicate(payload: &str) -> Result<bool, String> {
    let owned = payload.to_string();
    let (verdict, peak) = std::panic::catch_unwind(move || peak_alloc::measure(|| parse_where(&owned).map(|_| ())))
        .map_err(|_| "the decoder panicked".to_string())?;
    // A payload over the size cap is refused before it is read, holding only the refusal.
    let factor = if payload.len() > MAX_WHERE_BYTES { 0 } else { PREDICATE_FACTOR };
    peak_alloc::assert_within("parse_where", "a fuzzed predicate", payload.len(), peak, factor, PREDICATE_OVERHEAD);
    match verdict {
        Ok(()) => Ok(true),
        Err(error) => match CoreError::from(error) {
            CoreError::Refusal { code: RefusalCode::QueryPredicateInvalid, .. } => Ok(false),
            other => Err(format!("refused without the predicate code: {:?}", other)),
        },
    }
}

#[test]
fn predicates_are_accepted_exactly_when_the_grammar_accepts_them() {
    let mut rng = Rng::new(0xF01C_11F7_5EED_0426);
    let (mut accepted, mut refused) = (0usize, 0usize);

    for case in 0..30_000 {
        let mut tree = predicate_tree(&mut rng, 5);
        if case % 3 == 0 {
            respell_paths(&mut tree, &mut rng);
        }
        if case % 10 != 0 {
            for _ in 0..1 + rng.below(2) {
                mutate_predicate(&mut tree, &mut rng);
            }
        }
        let payload = tree.to_string();
        let expected = payload.len() <= MAX_WHERE_BYTES && grammar_accepts(&tree, 1, &mut 0);
        match decode_predicate(&payload) {
            Ok(verdict) if verdict == expected => {}
            Ok(verdict) => panic!("case {case}: the decoder {} what the grammar {}: {payload}",
                if verdict { "accepted" } else { "refused" }, if expected { "accepts" } else { "refuses" }),
            Err(what) => panic!("case {case}: {what}: {payload}"),
        }
        if expected { accepted += 1 } else { refused += 1 }
    }

    // Both sides of the property need a real domain.
    assert!(accepted >= 5_000 && refused >= 5_000, "accepted {accepted}, refused {refused}");
}

/// Write `node` as JSON, spelling the first key of the `target`-th object (in writing order) a
/// second time, with the value `again`.
fn write_with_duplicate(node: &Value, target: usize, again: &Value, seen: &mut usize, out: &mut String) {
    match node {
        Value::Object(object) => {
            let this = *seen;
            *seen += 1;
            out.push('{');
            for (index, (key, child)) in object.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_with_duplicate(child, target, again, seen, out);
            }
            if this == target {
                if let Some(key) = object.keys().next() {
                    out.push(',');
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    out.push_str(&again.to_string());
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, child) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_with_duplicate(child, target, again, seen, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[test]
fn a_predicate_naming_a_key_twice_is_refused_wherever_the_key_is() {
    let mut rng = Rng::new(0xF01C_11F7_5EED_D0B1);
    for case in 0..5_000 {
        let tree = predicate_tree(&mut rng, 5);
        let mut objects = 0;
        write_with_duplicate(&tree, usize::MAX, &Value::Null, &mut objects, &mut String::new());

        // The second spelling is sometimes the one that widens: an empty conjunction.
        let again = if rng.bool() { json!([]) } else { edge_value(&mut rng) };
        let mut payload = String::new();
        write_with_duplicate(&tree, rng.below(objects), &again, &mut 0, &mut payload);
        assert_eq!(decode_predicate(&payload), Ok(false), "case {case}: {payload}");
    }
}

#[test]
fn predicate_text_never_panics_the_decoder() {
    let mut rng = Rng::new(0xF01C_11F7_5EED_7E47);
    for case in 0..20_000 {
        let mut bytes = predicate_tree(&mut rng, 6).to_string().into_bytes();
        mutate(&mut bytes, &mut rng);
        let payload = String::from_utf8_lossy(&bytes);
        if let Err(what) = decode_predicate(&payload) {
            panic!("case {case}: {what}: {payload}");
        }
    }

    // Nesting inside the byte bound is refused by the JSON reader's own limit, before any tree
    // is built.
    let deep = format!("{}{}", "[".repeat(30_000), "]".repeat(30_000));
    assert!(deep.len() <= MAX_WHERE_BYTES);
    assert_eq!(decode_predicate(&deep), Ok(false));
}
