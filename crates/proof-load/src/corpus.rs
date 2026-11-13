//! The seeded corpus: a history of file trees generated from a seed, so the harness knows every
//! record's expected tree without asking the tool.
//!
//! The generator is the harness's own splitmix64, not a library's: a dependency update that changed
//! a library's stream would silently change what a seed means, and a seed in a report must replay
//! the same history for as long as the report is read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::oracle::{entry_digest, Tree};

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
    /// What the records say of each file's mode. Judged with nothing executable from
    /// [`generate`], so a seed means what it always meant; a scenario's own corpus marks the files
    /// it needs, or leaves modes unjudged.
    pub modes: Modes,
}

/// What a corpus says of its files' modes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Modes {
    /// Judged where the platform keeps modes ([`crate::oracle::MODES`]): these paths executable in
    /// every record that holds them, every other path not.
    Judged(BTreeSet<String>),
    /// Not judged at all, neither written nor read back: for a fault that is not about modes, run
    /// on a volume that may give every file a mode of its own, so its grade answers that fault alone.
    Unjudged,
}

impl Default for Modes {
    fn default() -> Modes {
        Modes::Judged(BTreeSet::new())
    }
}

impl Modes {
    /// Whether a file's mode is read and compared on this platform.
    pub fn judged(&self) -> bool {
        matches!(self, Modes::Judged(_)) && crate::oracle::MODES
    }

    /// Whether `path` is expected to read executable where modes are judged.
    pub fn executable(&self, path: &str) -> bool {
        matches!(self, Modes::Judged(executable) if executable.contains(path))
    }
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

    Corpus { seed, shape, records, modes: Modes::default() }
}

/// The tree a snapshot is expected to read back as under `modes`, read as
/// [`crate::oracle::read_tree`] reads with `modes.judged()`.
pub fn expected(snapshot: &Snapshot, modes: &Modes) -> Tree {
    expected_when(snapshot, modes, modes.judged())
}

/// [`expected`] where modes are `observed` or not: where they are not, as on Windows or for an
/// unjudged corpus, no file reads executable, so none is expected to.
pub fn expected_when(snapshot: &Snapshot, modes: &Modes, observed: bool) -> Tree {
    snapshot.iter().map(|(path, bytes)| (path.clone(), expected_entry_when(path, bytes, modes, observed))).collect()
}

/// The entry digest `path` holding `bytes` is expected to read back with under `modes`: the one
/// rule [`expected`] applies per file, for every other digest that must match a read tree's.
pub fn expected_entry(path: &str, bytes: &[u8], modes: &Modes) -> String {
    expected_entry_when(path, bytes, modes, modes.judged())
}

fn expected_entry_when(path: &str, bytes: &[u8], modes: &Modes, observed: bool) -> String {
    entry_digest(bytes, observed && modes.executable(path))
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

/// The one file a rooted corpus keeps outside its scope, the same in every record, so a sparse
/// warehouse carries something it does not hold.
pub const OUTSIDE: &str = "outside-scope.txt";

/// The corpus with every path moved under `scope`, plus `OUTSIDE` at the root. A scoped bay reads
/// and writes only its scope, and a seed's records need not change anything under any one folder
/// of the plain corpus, so a scenario with a scope records this one instead: each record changes
/// the scope, and something outside it stays.
pub fn rooted(corpus: &Corpus, scope: &str) -> Corpus {
    let records = corpus.records.iter().map(|snapshot| {
        let mut rooted: Snapshot = snapshot.iter().map(|(path, bytes)| (format!("{}/{}", scope, path), bytes.clone())).collect();
        rooted.insert(OUTSIDE.to_string(), format!("outside {}\n", scope).into_bytes());
        rooted
    }).collect();
    let modes = match &corpus.modes {
        Modes::Judged(executable) => Modes::Judged(executable.iter().map(|path| format!("{}/{}", scope, path)).collect()),
        Modes::Unjudged => Modes::Unjudged,
    };
    Corpus { records, modes, ..corpus.clone() }
}

/// The part of `snapshot` under `scope`: what a bay scoped to it holds.
pub fn within(snapshot: &Snapshot, scope: &str) -> Snapshot {
    let prefix = format!("{}/", scope);
    snapshot.iter().filter(|(path, _)| path.starts_with(&prefix)).map(|(path, bytes)| (path.clone(), bytes.clone())).collect()
}

/// Makes the working files under `root` match `snapshot`: writes every file it holds, and removes
/// every file it does not, leaving the tool's own state folder (`.forklift`, `.git`) alone. Where
/// `modes` are judged, each file it writes reads executable exactly where they mark it, so a bit a
/// tool left on a file is not carried into the next attempt; a mode is changed only where the file
/// reads otherwise. A refused change of mode fails the write, except on a folding volume
/// (`on_folding_volume`) refusing modes at all ([`crate::volume::refuses_modes`], vfat), which
/// keeps what it reads, since nothing written there can hold a bit. Unjudged modes are never
/// touched.
pub fn write(snapshot: &Snapshot, modes: &Modes, root: &Path, on_folding_volume: bool) -> std::io::Result<()> {
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
        std::fs::write(&target, bytes)?;
        if modes.judged() {
            set_executable(&target, modes.executable(path), on_folding_volume)?;
        }
    }
    Ok(())
}

/// Makes `file` read `executable` or not, changing its mode only where it reads otherwise; a
/// volume refusing modes is tolerated only where `refusal_tolerated`.
fn set_executable(file: &Path, executable: bool, refusal_tolerated: bool) -> std::io::Result<()> {
    let metadata = std::fs::metadata(file)?;
    if crate::oracle::executable_when(true, crate::oracle::mode(&metadata)) == executable {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        // Execute wherever read is allowed, as `chmod +x` under a usual umask; or execute nowhere.
        let mode = if executable { mode | 0o100 | ((mode & 0o044) >> 2) } else { mode & !0o111 };
        match std::fs::set_permissions(file, std::fs::Permissions::from_mode(mode)) {
            Err(error) if !(refusal_tolerated && crate::volume::refuses_modes(&error)) => return Err(error),
            _ => {}
        }
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
            write(snapshot, &Modes::default(), &root, false).unwrap();
            assert_eq!(crate::oracle::read_tree(&root).unwrap(), expected(snapshot, &Modes::default()));
        }
        assert_eq!(std::fs::read(root.join(".forklift/state")).unwrap(), b"tool state");
    }

    /// A path the corpus marks executable is written with the bit and expected with it, and a stray
    /// bit on any other path is cleared; a file that already reads as the corpus says gets no
    /// change of mode. Where modes are not observed, nothing is expected executable.
    ///
    /// Revert: chmod every marked path → the 0700 file becomes 0711, a mode change a volume
    /// without the bit refuses; never clear a bit → the stray executable keeps it.
    #[cfg(unix)]
    #[test]
    fn a_written_path_reads_executable_exactly_where_marked() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-corpus-modes-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&root).unwrap();
        let mode = |path: &str| std::fs::metadata(root.join(path)).unwrap().permissions().mode() & 0o777;
        let snapshot: Snapshot = [("run.sh", "#!/bin/sh\n"), ("kept.sh", "kept\n"), ("stray", "stray\n"), ("plain.txt", "plain\n")]
            .into_iter().map(|(path, bytes)| (path.to_string(), bytes.as_bytes().to_vec())).collect();
        let modes = Modes::Judged(["run.sh".to_string(), "kept.sh".to_string()].into());
        for (path, preset) in [("kept.sh", 0o700), ("stray", 0o755)] {
            std::fs::write(root.join(path), b"old").unwrap();
            std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(preset)).unwrap();
        }
        write(&snapshot, &modes, &root, false).unwrap();
        assert_eq!(mode("run.sh") & 0o100, 0o100);
        assert_eq!(mode("kept.sh"), 0o700, "already executable: no change of mode");
        assert_eq!(mode("stray"), 0o644, "a stray bit is cleared");
        assert_eq!(mode("plain.txt") & 0o111, 0);
        let read = crate::oracle::read_tree(&root).unwrap();
        let expected = expected(&snapshot, &modes);
        for path in ["run.sh", "kept.sh", "stray", "plain.txt"] {
            assert_eq!(read[path], expected[path], "{}", path);
        }
        assert_ne!(expected_when(&snapshot, &modes, true), expected_when(&snapshot, &modes, false));
        assert_eq!(expected_when(&snapshot, &modes, false), expected_when(&snapshot, &Modes::default(), true), "unobserved, no mark counts");
        assert_eq!(super::expected(&snapshot, &Modes::Unjudged), expected_when(&snapshot, &Modes::default(), true), "unjudged, nothing executable");

        // Unjudged modes are never touched: the stray bit stays, and the tree read without modes
        // matches.
        std::fs::set_permissions(root.join("stray"), std::fs::Permissions::from_mode(0o755)).unwrap();
        write(&snapshot, &Modes::Unjudged, &root, false).unwrap();
        assert_eq!(mode("stray"), 0o755, "unjudged: no change of mode");
        assert_eq!(crate::oracle::read_tree_when(&root, false).unwrap(), super::expected(&snapshot, &Modes::Unjudged));
    }

    /// A refused change of mode fails the write off a folding volume, and is tolerated on one: an
    /// immutable file (`chflags uchg`) answers its owner's chmod with EPERM, as vfat does.
    ///
    /// Revert: tolerate a refusal everywhere → the immutable file keeps its stray bit and the
    /// write passes, so the tool would be graded over a work tree the harness never wrote.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_change_of_mode_fails_off_a_folding_volume() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-corpus-refused-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("stray");
        std::fs::write(&file, b"stray\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        let flags = |flag: &str| assert!(std::process::Command::new("chflags").arg(flag).arg(&file).status().unwrap().success());
        flags("uchg");
        let off = set_executable(&file, false, false);
        let on = set_executable(&file, false, true);
        flags("nouchg");
        assert_eq!(off.unwrap_err().raw_os_error(), Some(1), "EPERM fails the write off a folding volume");
        assert!(on.is_ok(), "and is tolerated on one");
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

    /// A rooted corpus changes its scope in every record of every seed, keeps one file outside it,
    /// and a bay scoped to it holds every rooted path but that one.
    ///
    /// Revert: root only the paths already under the scope → some seed records nothing new there.
    #[test]
    fn a_rooted_corpus_changes_its_scope_in_every_record() {
        for seed in 0..500 {
            let rooted = rooted(&generate(seed, Shape { records: 4, ..Shape::SMALL }), "src");
            for pair in rooted.records.windows(2) {
                assert_ne!(within(&pair[0], "src"), within(&pair[1], "src"), "seed {}", seed);
            }
            for snapshot in &rooted.records {
                let scoped = within(snapshot, "src");
                assert_eq!(scoped.len() + 1, snapshot.len(), "seed {}: only {} lies outside", seed, OUTSIDE);
                assert!(snapshot.contains_key(OUTSIDE) && !scoped.contains_key(OUTSIDE));
            }
        }
        assert!(within(&rooted(&generate(1, Shape::SMALL), "src").records[0], "sr").is_empty(), "a scope is a folder, not a prefix");
    }
}
