//! The seeded corpus: a history of file trees generated from a seed, so the harness knows every
//! record's expected tree without asking the tool.
//!
//! The generator is the harness's own splitmix64, not a library's: a dependency update that changed
//! a library's stream would silently change what a seed means, and a seed in a report must replay
//! the same history for as long as the report is read.

use std::collections::BTreeMap;
use std::path::Path;

use crate::oracle::{digest, Tree};

/// splitmix64 (Steele, Lea and Flood, 2014), the whole of the harness's randomness.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`; `bound` must be non-zero.
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "below(0) has no value");
        self.next_u64() % bound
    }
}

/// One record's content: each file's path (relative, `/`-separated) and its bytes.
pub type Snapshot = BTreeMap<String, Vec<u8>>;

/// The size of a generated history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Shape {
    pub records: usize,
    /// Files in the first record.
    pub initial_files: usize,
    /// Upper bound on one file's size, in bytes.
    pub max_bytes: usize,
}

impl Shape {
    /// The one small history the per-PR lane runs every damaged-file binding against.
    pub const SMALL: Shape = Shape { records: 3, initial_files: 6, max_bytes: 4096 };
}

/// A generated history: the snapshot each record is expected to hold, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corpus {
    pub seed: u64,
    pub shape: Shape,
    pub records: Vec<Snapshot>,
}

const DIRECTORIES: &[&str] = &["", "docs/", "src/", "src/nested/", "assets/"];

fn path(rng: &mut Rng, counter: &mut u64) -> String {
    let directory = DIRECTORIES[rng.below(DIRECTORIES.len() as u64) as usize];
    *counter += 1;
    let extension = if rng.below(4) == 0 { "bin" } else { "txt" };
    format!("{}file-{}.{}", directory, counter, extension)
}

/// Content of `path`: text lines for a `.txt` file, so merges and diffs see lines, and arbitrary
/// bytes for a `.bin` file. Never empty, so every file contributes an object.
fn content(rng: &mut Rng, path: &str, max_bytes: usize) -> Vec<u8> {
    let length = 1 + rng.below(max_bytes.max(1) as u64) as usize;
    if path.ends_with(".bin") {
        return (0..length).map(|_| rng.next_u64() as u8).collect();
    }
    let mut bytes = Vec::with_capacity(length + 32);
    while bytes.len() < length {
        let words = 1 + rng.below(8);
        for word in 0..words {
            if word > 0 {
                bytes.push(b' ');
            }
            bytes.extend_from_slice(format!("w{:x}", rng.below(1 << 16)).as_bytes());
        }
        bytes.push(b'\n');
    }
    bytes
}

/// Generates the history `seed` names. Every record after the first changes the tree: it modifies
/// at least one file, and may add and delete others.
pub fn generate(seed: u64, shape: Shape) -> Corpus {
    let mut rng = Rng::new(seed);
    let mut counter = 0;
    let mut records = Vec::with_capacity(shape.records);

    let mut current = Snapshot::new();
    while current.len() < shape.initial_files.max(1) {
        let name = path(&mut rng, &mut counter);
        let bytes = content(&mut rng, &name, shape.max_bytes);
        current.insert(name, bytes);
    }
    if shape.records > 0 {
        records.push(current.clone());
    }

    while records.len() < shape.records {
        let names: Vec<String> = current.keys().cloned().collect();
        let modified = names[rng.below(names.len() as u64) as usize].clone();
        let mut bytes = content(&mut rng, &modified, shape.max_bytes);
        if current.get(&modified) == Some(&bytes) {
            bytes.push(b'\n');
        }
        current.insert(modified.clone(), bytes);

        if names.len() > 1 && rng.below(2) == 0 {
            let deleted = &names[rng.below(names.len() as u64) as usize];
            if *deleted != modified {
                current.remove(deleted);
            }
        }
        for _ in 0..rng.below(3) {
            let name = path(&mut rng, &mut counter);
            let bytes = content(&mut rng, &name, shape.max_bytes);
            current.insert(name, bytes);
        }
        records.push(current.clone());
    }

    Corpus { seed, shape, records }
}

/// The tree a snapshot is expected to read back as.
pub fn expected(snapshot: &Snapshot) -> Tree {
    snapshot.iter().map(|(path, bytes)| (path.clone(), digest(bytes))).collect()
}

/// The variant of a snapshot: every file changed, a text file's first line replaced and a binary
/// file's last byte inverted, so a branch recording it and another recording the next corpus record
/// change at least one file two ways. A record after the first always modifies a file the one
/// before holds, and a modification rewrites the whole file, first line included.
pub fn variant(snapshot: &Snapshot) -> Snapshot {
    snapshot.iter().map(|(path, bytes)| {
        let mut bytes = bytes.clone();
        if path.ends_with(".bin") {
            if let Some(last) = bytes.last_mut() {
                *last = !*last;
            }
        } else {
            let rest = bytes.iter().position(|byte| *byte == b'\n').map_or(Vec::new(), |end| bytes[end + 1..].to_vec());
            bytes = b"proof-load variant\n".iter().copied().chain(rest).collect();
        }
        (path.clone(), bytes)
    }).collect()
}

/// Makes the working files under `root` match `snapshot`: writes every file it holds, and removes
/// every file it does not, leaving the tool's own state folder (`.forklift`, `.git`) alone.
pub fn write(snapshot: &Snapshot, root: &Path) -> std::io::Result<()> {
    for (path, _) in crate::oracle::read_tree(root)? {
        if !snapshot.contains_key(&path) {
            #[allow(clippy::disallowed_methods)] // a scratch work tree the harness wrote; no retry is wanted
            std::fs::remove_file(root.join(&path))?;
        }
    }
    for (path, bytes) in snapshot {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first values of splitmix64 from seed 0, as published with the algorithm's reference
    /// implementation; a changed constant changes what every reported seed means.
    ///
    /// Revert: change any multiplier → the first value.
    #[test]
    fn the_generator_is_splitmix64() {
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    /// A seed names one history; another seed names another; every record changes the tree.
    ///
    /// Revert: drop the newline nudge for an unchanged rewrite → some seed's record repeats.
    #[test]
    fn a_seed_names_one_history_and_every_record_changes_it() {
        let shape = Shape { records: 6, initial_files: 4, max_bytes: 1 };
        assert_eq!(generate(7, shape), generate(7, shape));
        assert_ne!(generate(7, shape).records, generate(8, shape).records);

        for seed in 0..4000 {
            let corpus = generate(seed, shape);
            assert_eq!(corpus.records.len(), 6);
            for pair in corpus.records.windows(2) {
                assert_ne!(pair[0], pair[1], "seed {} has a record that changes nothing", seed);
            }
            for snapshot in &corpus.records {
                assert!(!snapshot.is_empty() && snapshot.values().all(|bytes| !bytes.is_empty()));
                assert!(snapshot.keys().all(|path| !path.starts_with('.')), "no generated path enters a tool's state folder");
            }
        }
    }

    /// Writing a record over the previous one leaves exactly that record's tree, the tool's state
    /// folder untouched.
    ///
    /// Revert: skip the removal pass → a file the record deleted survives in the tree.
    #[test]
    fn a_written_record_reads_back_as_its_expected_tree() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-corpus-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join(".forklift")).unwrap();
        std::fs::write(root.join(".forklift/state"), b"tool state").unwrap();

        let corpus = (0..).map(|seed| generate(seed, Shape { records: 4, initial_files: 5, max_bytes: 64 }))
            .find(|corpus| corpus.records.windows(2).any(|pair| pair[0].keys().any(|path| !pair[1].contains_key(path))))
            .unwrap();
        for snapshot in &corpus.records {
            write(snapshot, &root).unwrap();
            assert_eq!(crate::oracle::read_tree(&root).unwrap(), expected(snapshot));
        }
        assert_eq!(std::fs::read(root.join(".forklift/state")).unwrap(), b"tool state");
    }

    /// A variant changes every file of its record and keeps its paths, a binary one at its length,
    /// so a side branch recording it conflicts with any later change to the same file.
    ///
    /// Revert: leave a `.bin` file as it is → that file is unchanged.
    #[test]
    fn a_variant_changes_every_file_and_keeps_its_paths() {
        for seed in 0..2000 {
            for snapshot in &generate(seed, Shape { records: 3, initial_files: 4, max_bytes: 16 }).records {
                let varied = variant(snapshot);
                assert_eq!(varied.keys().collect::<Vec<_>>(), snapshot.keys().collect::<Vec<_>>());
                for (path, bytes) in snapshot {
                    assert_ne!(&varied[path], bytes, "seed {} leaves {} unchanged", seed, path);
                    if path.ends_with(".bin") {
                        assert_eq!(varied[path].len(), bytes.len());
                    }
                }
            }
        }
    }
}
