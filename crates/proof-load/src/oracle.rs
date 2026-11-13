//! The oracle and the grades.
//!
//! The oracle is the seed: the harness generated every file's content, so it knows the expected
//! tree at every record without asking the tool. A readback is a tree the tool itself checked out
//! or exported into a fresh directory, compared by bytes; the harness never decodes a store.
//!
//! The record the faulted command was making expects either of two trees, the state before that
//! command and the state after it. Every record acknowledged before the fault expects one tree,
//! except where a declared transform (redaction, scope prune, store evict, server withhold) took
//! content away by design; for those objects what is admissible depends on the transform's state in
//! the warehouse serving the read, at the point being graded.

use std::collections::BTreeMap;

/// A tree: each file's path (relative, `/`-separated) and its entry digest ([`entry_digest`]).
pub type Tree = BTreeMap<String, String>;

/// Hashes `bytes` the way trees record a normal file.
pub fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// What an executable file's entry digest covers ahead of its bytes.
const EXECUTABLE: &[u8] = b"proof-load mode 100755\0";

/// Hashes one tree entry: a normal file's bytes as [`digest`] does, so every tree of files without
/// the executable bit keeps the digests it always had; an executable file's mode marker and bytes,
/// so a record that lost or gained the bit reads as a different tree.
pub fn entry_digest(bytes: &[u8], executable: bool) -> String {
    if !executable {
        return digest(bytes);
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(EXECUTABLE);
    hasher.update(bytes);
    hasher.finalize().to_hex().to_string()
}

/// Whether trees on this platform carry the executable bit: Unix keeps it in the file mode, and
/// Windows has none to read, so there every file reads normal and no expectation marks one.
pub const MODES: bool = cfg!(unix);

/// Whether a file of `mode` (None where the platform gives none) reads executable, where modes are
/// `observed`: its owner may execute it. Split from the platform so both arms are tested anywhere.
pub fn executable_when(observed: bool, mode: Option<u32>) -> bool {
    observed && mode.is_some_and(|mode| mode & 0o100 != 0)
}

/// The mode of the file `metadata` describes, on a platform that has one.
pub fn mode(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    return Some(std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()));
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// Reads the tree under `root`, skipping the tool's own state folder (`.forklift`, `.git`), each
/// file's mode read from disk where the platform keeps one.
pub fn read_tree(root: &std::path::Path) -> std::io::Result<Tree> {
    let mut tree = Tree::new();
    for path in crate::census::walk(root)? {
        let first = path.split('/').next().unwrap_or("");
        if path.ends_with('/') || first == ".forklift" || first == ".git" {
            continue;
        }
        let file = root.join(&path);
        // Only a regular file carries the bit: a link's own mode reads 0o777, and its bytes are
        // read through it.
        let metadata = std::fs::symlink_metadata(&file)?;
        let executable = executable_when(MODES && metadata.is_file(), mode(&metadata));
        tree.insert(path.clone(), entry_digest(&std::fs::read(&file)?, executable));
    }
    Ok(tree)
}

/// What a read of a path may legitimately produce.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Admissible {
    /// The bytes the record holds.
    Recorded { digest: String },
    /// The documented behaviour of a read of taken-away content: a refusal with this code, or, for
    /// a host's withhold, its statement served in place of the content.
    Documented { code: String },
}

/// What a readback of one record may produce: a set of whole trees, each path of which is either
/// fixed or open to the transform-dependent alternatives.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Expectation {
    /// The admissible trees. One for an acknowledged record, two (before, after) for the record
    /// the faulted command was making.
    pub trees: Vec<Tree>,
    /// Paths a declared transform reached, with every alternative now admissible for each.
    pub transformed: BTreeMap<String, Vec<Admissible>>,
}

impl Expectation {
    pub fn acknowledged(tree: Tree) -> Expectation {
        Expectation { trees: vec![tree], transformed: BTreeMap::new() }
    }

    /// The faulted command's own record: a kill may legitimately leave either tree.
    pub fn faulted(before: Tree, after: Tree) -> Expectation {
        Expectation { trees: vec![before, after], transformed: BTreeMap::new() }
    }
}

/// What a readback produced.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Readback {
    /// Exit 0: the tree it wrote, and per path any documented code it reported in place of bytes.
    Served { tree: Tree, documented: BTreeMap<String, String> },
    /// A non-zero exit, with the error code it reported.
    Refused { code: Option<String> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// Exit 0 with bytes matching an admissible tree.
    Expected,
    /// Exit 0 with bytes matching none. Takes precedence over every other outcome.
    Silent,
    /// A refusal: whether that is healed, remedied, lost or a failure is decided across steps.
    Refused,
}

/// Judges one readback against its expectation.
pub fn judge(readback: &Readback, expectation: &Expectation) -> Verdict {
    let (tree, documented) = match readback {
        Readback::Refused { .. } => return Verdict::Refused,
        Readback::Served { tree, documented } => (tree, documented),
    };

    let path_fits = |path: &str, fixed: Option<&String>| -> bool {
        let served = tree.get(path);
        let reported = documented.get(path);
        match expectation.transformed.get(path) {
            Some(alternatives) => alternatives.iter().any(|alternative| match alternative {
                Admissible::Recorded { digest } => served == Some(digest) && reported.is_none(),
                Admissible::Documented { code } => reported == Some(code),
            }),
            None => reported.is_none() && served == fixed,
        }
    };

    let fits = expectation.trees.iter().any(|expected| {
        let paths: std::collections::BTreeSet<&String> = expected.keys().chain(tree.keys()).chain(documented.keys())
            .chain(expectation.transformed.keys()).collect();
        paths.into_iter().all(|path| path_fits(path, expected.get(path)))
    });

    if fits { Verdict::Expected } else { Verdict::Silent }
}

/// The five grades.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Grade {
    /// Exit 0 with expected bytes, in the faulted command or a plain retry of it.
    Healed,
    /// A refusal naming a remedy which, once run, leads to expected bytes.
    RefusedWorkingRemedy,
    /// A refusal naming no remedy, or one that fails, while every earlier record reads back. A failure.
    RefusedNoRemedy,
    /// Some readback served bytes matching no expected tree with exit 0. A failure; it wins.
    Silent,
    /// After every named remedy, heal included, an acknowledged record cannot be read back and the
    /// tool refuses rather than serving wrong bytes.
    DataLost,
}

impl Grade {
    pub fn is_failure(self) -> bool {
        matches!(self, Grade::RefusedNoRemedy | Grade::Silent | Grade::DataLost)
    }
}

/// What a scenario's steps established, from which its grade follows.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Evidence {
    /// Any readback, at any graded point, judged silent.
    pub any_silent: bool,
    /// The faulted command or a plain retry exited 0 and the readback after it was expected.
    pub healed_by_retry: bool,
    /// The refusal named at least one extractable remedy.
    pub remedy_named: bool,
    /// Running the named remedies led to expected bytes for the faulted record.
    pub remedy_worked: bool,
    /// After every named remedy, heal included, every record acknowledged before the fault reads back.
    pub acknowledged_readable: bool,
}

pub fn grade(evidence: &Evidence) -> Grade {
    if evidence.any_silent {
        Grade::Silent
    } else if !evidence.acknowledged_readable {
        Grade::DataLost
    } else if evidence.healed_by_retry {
        Grade::Healed
    } else if evidence.remedy_named && evidence.remedy_worked {
        Grade::RefusedWorkingRemedy
    } else {
        Grade::RefusedNoRemedy
    }
}

/// Each grade's recoverability record.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Recoverability {
    /// Commands from the fault to expected bytes, or none if expected bytes were never reached.
    pub commands_to_expected: Option<u32>,
    /// Whether the tool named the remedy that worked.
    pub named_right_remedy: bool,
    /// Steps that are not the tool's own commands (removing a file, freeing space, lifting a fault).
    pub outside_knowledge: Vec<String>,
}

/// A redaction's state in one warehouse, as that warehouse reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RedactionState {
    /// Listed pending by `redact list`, a redactions_unsettled warning, or the handshake's Pending.
    Pending,
    /// Listed applied by `redact list`; its copies may still be present.
    Applied,
    /// The handshake's DeletionOwed, or `redact apply --json` reporting a deletion owed.
    DeletionOwed,
    /// The handshake's Deleted, or `redact apply --json` reporting no deletion owed.
    Deleted,
}

/// Every redaction's state per warehouse across a scenario's graded points. An applied redaction
/// is never taken back, so a warehouse listing one pending again after it was applied is silent.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RedactionLedger {
    /// (warehouse, redaction) → the furthest state seen.
    states: BTreeMap<(String, String), RedactionState>,
}

impl RedactionLedger {
    /// Records what `warehouse` reports for `redaction` at a graded point. Returns `Verdict::Silent`
    /// when the report goes back on an earlier one.
    pub fn observe(&mut self, warehouse: &str, redaction: &str, state: RedactionState) -> Verdict {
        let key = (warehouse.to_string(), redaction.to_string());
        let seen = self.states.get(&key).copied();
        if state == RedactionState::Pending && seen.is_some_and(|seen| seen >= RedactionState::Applied) {
            return Verdict::Silent;
        }
        // A later report of a state short of one already seen (`redact list` saying Applied after an
        // apply reported the copies deleted) is not a regression: the two sources report different
        // facts. The ledger keeps the furthest.
        if seen.is_none_or(|seen| state > seen) {
            self.states.insert(key, state);
        }
        Verdict::Expected
    }

    /// What a read of a redacted object in `warehouse` may produce: both the recorded bytes and the
    /// documented behaviour until that warehouse reports no deletion owed, then the documented
    /// behaviour only.
    pub fn admissible(&self, warehouse: &str, redaction: &str, recorded: &str, code: &str) -> Vec<Admissible> {
        let documented = Admissible::Documented { code: code.to_string() };
        match self.states.get(&(warehouse.to_string(), redaction.to_string())) {
            Some(RedactionState::Deleted) => vec![documented],
            Some(_) => vec![Admissible::Recorded { digest: recorded.to_string() }, documented],
            None => vec![Admissible::Recorded { digest: recorded.to_string() }],
        }
    }
}

/// Who serves a read of withheld content, which decides what it expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WithholdReader {
    /// The host itself: its statement.
    Host,
    /// A client that fetched after the withhold: its own `content_withheld` refusal.
    FetchedAfter,
    /// A client that held the content before: its copy.
    HeldBefore,
}

/// What a read of withheld content may produce, once `withhold add` was acknowledged.
pub fn withheld_admissible(reader: WithholdReader, recorded: &str) -> Vec<Admissible> {
    match reader {
        WithholdReader::Host => vec![Admissible::Documented { code: "withheld_statement".to_string() }],
        WithholdReader::FetchedAfter => vec![Admissible::Documented { code: "content_withheld".to_string() }],
        WithholdReader::HeldBefore => vec![Admissible::Recorded { digest: recorded.to_string() }],
    }
}

/// What a read of content a prune or an evict reached may produce: they resume after an
/// interruption and a read fetches the content back, so the recorded bytes stay admissible.
pub fn freed_admissible(recorded: &str) -> Vec<Admissible> {
    vec![Admissible::Recorded { digest: recorded.to_string() }]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A normal file's entry digest is the digest of its bytes, unchanged; an executable file's is
    /// another, so a tree that lost or gained the bit reads as a different tree. Read from disk, the
    /// mode decides it where modes are observed, and nowhere else.
    ///
    /// Revert: make the entry digest ignore the mode → the executable and the normal tree compare
    /// equal, and a record that lost run.sh's bit grades as healed.
    #[test]
    fn the_entry_digest_covers_the_executable_bit() {
        assert_eq!(entry_digest(b"x", false), digest(b"x"));
        assert_ne!(entry_digest(b"x", true), digest(b"x"));
        assert_ne!(entry_digest(b"x", true), entry_digest(b"y", true));
        assert!(executable_when(true, Some(0o100755)) && executable_when(true, Some(0o100700)));
        assert!(!executable_when(true, Some(0o100644)) && !executable_when(true, Some(0o100011)));
        assert!(!executable_when(false, Some(0o100755)), "where modes are not observed, nothing reads executable");
        assert!(!executable_when(true, None));
    }

    /// The same bytes read back as two trees once one of them carries the bit; a link reads as its
    /// target's bytes and never as executable, whatever its own mode.
    ///
    /// Revert: read the bytes only → the two folders read the same; read the bit off a link too →
    /// the link, whose own mode is 0o755 or 0o777, reads executable over a plain file.
    #[cfg(unix)]
    #[test]
    fn read_tree_sees_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-oracle-modes-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("run.sh", root.join("link")).unwrap();
        let normal = read_tree(&root).unwrap();
        assert_eq!(normal["run.sh"], digest(b"#!/bin/sh\n"));
        assert_eq!(normal["link"], digest(b"#!/bin/sh\n"), "a link carries no bit of its own");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let executable = read_tree(&root).unwrap();
        assert_eq!(executable["run.sh"], entry_digest(b"#!/bin/sh\n", true));
        assert_ne!(normal, executable);
    }

    fn tree(entries: &[(&str, &str)]) -> Tree {
        entries.iter().map(|(path, bytes)| (path.to_string(), digest(bytes.as_bytes()))).collect()
    }

    fn served(entries: &[(&str, &str)]) -> Readback {
        Readback::Served { tree: tree(entries), documented: BTreeMap::new() }
    }

    /// Revert: accept only the after tree → the early kill's old history reads silent; accept any
    /// subset → the mixed tree passes.
    #[test]
    fn the_faulted_record_admits_either_tree_and_nothing_between() {
        let before = tree(&[("a", "1"), ("b", "1")]);
        let after = tree(&[("a", "2"), ("b", "2")]);
        let expectation = Expectation::faulted(before, after);

        assert_eq!(judge(&served(&[("a", "1"), ("b", "1")]), &expectation), Verdict::Expected);
        assert_eq!(judge(&served(&[("a", "2"), ("b", "2")]), &expectation), Verdict::Expected);
        assert_eq!(judge(&served(&[("a", "2"), ("b", "1")]), &expectation), Verdict::Silent);
        assert_eq!(judge(&served(&[("a", "1")]), &expectation), Verdict::Silent);
        assert_eq!(judge(&served(&[("a", "1"), ("b", "1"), ("c", "")]), &expectation), Verdict::Silent);
        assert_eq!(judge(&Readback::Refused { code: None }, &expectation), Verdict::Refused);
    }

    /// Silent wins over everything, and data lost over the refusal grades.
    ///
    /// Revert: check `acknowledged_readable` before `any_silent` → the first case grades data lost.
    #[test]
    fn grade_precedence() {
        let silent = Evidence { any_silent: true, acknowledged_readable: false, healed_by_retry: true, ..Evidence::default() };
        assert_eq!(grade(&silent), Grade::Silent);
        let lost = Evidence { acknowledged_readable: false, remedy_named: true, remedy_worked: true, ..Evidence::default() };
        assert_eq!(grade(&lost), Grade::DataLost);
        let healed = Evidence { acknowledged_readable: true, healed_by_retry: true, ..Evidence::default() };
        assert_eq!(grade(&healed), Grade::Healed);
        let remedied = Evidence { acknowledged_readable: true, remedy_named: true, remedy_worked: true, ..Evidence::default() };
        assert_eq!(grade(&remedied), Grade::RefusedWorkingRemedy);
        let stuck = Evidence { acknowledged_readable: true, remedy_named: true, remedy_worked: false, ..Evidence::default() };
        assert_eq!(grade(&stuck), Grade::RefusedNoRemedy);
        assert!(grade(&stuck).is_failure() && !grade(&remedied).is_failure());
    }

    /// Recorded bytes stay admissible beside the documented behaviour until the warehouse reports
    /// no deletion owed; after that, recorded bytes served are silent.
    ///
    /// Revert: drop recorded bytes at Applied → a kept copy after an apply that found the store in
    /// use reads silent; keep them at Deleted → a reverted deletion passes.
    #[test]
    fn a_redacted_object_admits_its_bytes_until_deletion_is_reported() {
        let recorded = digest(b"secret");
        let mut ledger = RedactionLedger::default();
        let read = |admissible: Vec<Admissible>, readback: Readback| {
            let expectation = Expectation { trees: vec![tree(&[("keep", "k")])], transformed: BTreeMap::from([("secret.txt".to_string(), admissible)]) };
            judge(&readback, &expectation)
        };
        let bytes = || served(&[("keep", "k"), ("secret.txt", "secret")]);
        let refused = || Readback::Served { tree: tree(&[("keep", "k")]), documented: BTreeMap::from([("secret.txt".to_string(), "redacted".to_string())]) };

        assert_eq!(ledger.observe("client", "r1", RedactionState::Applied), Verdict::Expected);
        assert_eq!(read(ledger.admissible("client", "r1", &recorded, "redacted"), bytes()), Verdict::Expected);
        assert_eq!(read(ledger.admissible("client", "r1", &recorded, "redacted"), refused()), Verdict::Expected);

        assert_eq!(ledger.observe("client", "r1", RedactionState::Deleted), Verdict::Expected);
        assert_eq!(read(ledger.admissible("client", "r1", &recorded, "redacted"), bytes()), Verdict::Silent);
        assert_eq!(read(ledger.admissible("client", "r1", &recorded, "redacted"), refused()), Verdict::Expected);

        // Each warehouse is asked on its own: the host's state is not the client's.
        assert_eq!(read(ledger.admissible("host", "r1", &recorded, "redacted"), bytes()), Verdict::Expected);
    }

    /// Revert: let a later report lower the state → the regression passes as expected.
    #[test]
    fn an_applied_redaction_listed_pending_again_is_silent() {
        let mut ledger = RedactionLedger::default();
        assert_eq!(ledger.observe("client", "r1", RedactionState::Pending), Verdict::Expected);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Applied), Verdict::Expected);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Applied), Verdict::Expected);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Pending), Verdict::Silent);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Deleted), Verdict::Expected);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Applied), Verdict::Expected);
        assert_eq!(ledger.admissible("client", "r1", "d", "redacted"), vec![Admissible::Documented { code: "redacted".to_string() }]);
        assert_eq!(ledger.observe("client", "r1", RedactionState::Pending), Verdict::Silent);
    }

    /// Revert: give every reader the host's statement → a client holding its copy reads silent.
    #[test]
    fn withhold_expects_by_who_serves_the_read() {
        let recorded = digest(b"withheld");
        assert_eq!(withheld_admissible(WithholdReader::HeldBefore, &recorded), vec![Admissible::Recorded { digest: recorded.clone() }]);
        assert_eq!(withheld_admissible(WithholdReader::FetchedAfter, &recorded), vec![Admissible::Documented { code: "content_withheld".to_string() }]);
        assert!(!withheld_admissible(WithholdReader::Host, &recorded).contains(&Admissible::Recorded { digest: recorded }));
    }
}
