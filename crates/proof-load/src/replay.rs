//! Merge replay: the two-parent merges of a real history, all of them or a seeded sample, each
//! replayed in Forklift from its two parents and compared path by path with git's merge of the
//! same two commits, then judged against crates/proof-load/replay-tickets.json.
//!
//! Every path where Forklift's result differs from git's gets exactly one kind. Two kinds are gaps,
//! each a feature Forklift lacks and git has: a rename gap, where turning git's rename detection off
//! changes git's result there, and a multi-base gap, where merging against one base instead of git's
//! virtual base changes it. Each is shown by a counterfactual git merge in the same mirror, never
//! inferred. Any other difference is a divergence from Forklift's own model of a merge, which git
//! expresses as a no-renames merge against one base, and nothing absorbs it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::report::Platform;
use crate::scrub::{Environment, Output, Scratch};
use crate::tool::{self, Invocation, Outcome};

/// The replay.json format this build writes.
pub const REPLAY_FORMAT: u32 = 1;

/// How long the mirror, its rewrite and the import may each take.
const BUILD_LIMIT: Duration = Duration::from_secs(30 * 60);
/// How long one command of one row may take.
const STEP_LIMIT: Duration = Duration::from_secs(10 * 60);
/// The branch the harness makes at the resolved ref, the mirror's HEAD, so the import makes it
/// current whatever the source's HEAD was.
const HEAD_BRANCH: &str = "proof-load-replay";
/// How many paths of one row get a hunk in the report.
const HUNKS_PER_ROW: usize = 5;
/// How many lines of one hunk the report keeps.
const HUNK_LINES: usize = 40;
/// The ticket the .forkliftignore drop waits on: a revision whose .forkliftignore predates the
/// current dialect can be shifted onto but not away from. The drop goes when it closes.
const IGNORE_TICKET: &str = "FORK-536";
/// git's empty tree, which every repository resolves without holding it.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// What `proof-load replay` was asked to run.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// A git repository, read only: mirrored into the scratch area first.
    pub source: PathBuf,
    /// Any revision git resolves to a commit in `source`; a detached HEAD and a commit id included.
    pub reference: String,
    pub out: PathBuf,
    pub forklift: PathBuf,
    pub git: PathBuf,
    /// How many merges to replay; all of them without it.
    pub sample: Option<usize>,
    /// Shuffles the merge list before it is sampled.
    pub seed: Option<u64>,
}

/// One kind of difference at a path, or of a row a ticket entry can name. The first five are the
/// divergence subtypes, which path entries name; record-mismatch is the one a row has with an
/// empty path and blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Clean in both Forklift and G0, with different files.
    SilentWrong,
    /// Clean in Forklift, conflicted in G0.
    ForkliftCleanGitConflicted,
    /// Conflicted in Forklift, clean in G0.
    ForkliftExtraConflict,
    /// Conflicted in both, with different files once the marker labels are stripped.
    ConflictContent,
    /// Forklift's working tree is not what its consolidate recorded.
    RecordMismatch,
    ForkliftRefused,
    Error,
}

impl Kind {
    /// Whether an entry of this kind names a row, by text its reason contains.
    pub fn row_level(self) -> bool {
        matches!(self, Kind::ForkliftRefused | Kind::Error)
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::SilentWrong => "silent-wrong",
            Kind::ForkliftCleanGitConflicted => "forklift-clean-git-conflicted",
            Kind::ForkliftExtraConflict => "forklift-extra-conflict",
            Kind::ConflictContent => "conflict-content",
            Kind::RecordMismatch => "record-mismatch",
            Kind::ForkliftRefused => "forklift-refused",
            Kind::Error => "error",
        }
    }
}

/// A feature Forklift lacks that a gap is attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cause {
    Rename,
    MultiBase,
}

impl Cause {
    pub fn label(self) -> &'static str {
        match self {
            Cause::Rename => "rename",
            Cause::MultiBase => "multi-base",
        }
    }
}

/// Which way a gap goes: whether Forklift conflicted at the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    ForkliftConflicted,
    ForkliftClean,
}

/// The kind of one differing path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Difference {
    /// Forklift differs from G0 there.
    Divergence { subtype: Kind },
    /// Forklift equals G0 there, and G and G1 differ there.
    RenameGap { direction: Direction },
    /// Forklift equals G0 there, G and G1 agree, and G1 and G0 differ.
    MultiBaseGap { direction: Direction },
}

impl Difference {
    pub fn is_divergence(&self) -> bool {
        matches!(self, Difference::Divergence { .. })
    }
}

/// One path of one result: whether that result conflicted there, and its file's value. A clean
/// file's value is its mode and blob, a conflicted file's is its content with the marker labels
/// stripped, and an absent file's is that it is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathState {
    pub conflicted: bool,
    pub value: String,
}

impl PathState {
    /// The value of an absent path.
    pub const ABSENT: &'static str = "absent";

    /// Whether the path differs between two results: conflicted in one only, or with different
    /// values.
    pub fn differs(&self, other: &PathState) -> bool {
        self.conflicted != other.conflicted || self.value != other.value
    }
}

/// The kind of a path in Forklift's result `forklift` against git's real merge `g`, the same
/// merge without rename detection `g1`, and that merge against one base `g0`; None when Forklift
/// differs from neither G0 nor G there. Divergence is decided first: a path where Forklift differs
/// from its own model, G0, is never explained by a feature it lacks.
pub fn difference(forklift: &PathState, g: &PathState, g1: &PathState, g0: &PathState) -> Option<Difference> {
    let direction = if forklift.conflicted { Direction::ForkliftConflicted } else { Direction::ForkliftClean };
    if forklift.differs(g0) {
        let subtype = match (forklift.conflicted, g0.conflicted) {
            (false, false) => Kind::SilentWrong,
            (false, true) => Kind::ForkliftCleanGitConflicted,
            (true, false) => Kind::ForkliftExtraConflict,
            (true, true) => Kind::ConflictContent,
        };
        return Some(Difference::Divergence { subtype });
    }
    if !forklift.differs(g) {
        return None;
    }
    if g.differs(g1) {
        return Some(Difference::RenameGap { direction });
    }
    // Forklift equals G0 and differs from G, and G equals G1, so G1 and G0 differ here.
    Some(Difference::MultiBaseGap { direction })
}

/// `content` with the label text after each `<<<<<<<`, `|||||||` and `>>>>>>>` marker removed,
/// so two conflicted files that differ only in what their sides are called compare equal.
pub fn strip_labels(content: &[u8]) -> Vec<u8> {
    let mut stripped = Vec::with_capacity(content.len());
    for line in content.split_inclusive(|byte| *byte == b'\n') {
        let (body, end) = match line.strip_suffix(b"\n") {
            Some(body) => (body, &b"\n"[..]),
            None => (line, &b""[..]),
        };
        let marker = [&b"<<<<<<<"[..], b"|||||||", b">>>>>>>"].into_iter()
            .find(|marker| body.starts_with(marker) && (body.len() == marker.len() || body[marker.len()] == b' '));
        match marker {
            Some(marker) => {
                stripped.extend_from_slice(marker);
                stripped.extend_from_slice(end);
            }
            None => stripped.extend_from_slice(line),
        }
    }
    stripped
}

/// The value of a conflicted file: a digest of its content with the marker labels stripped.
pub fn conflicted_value(content: &[u8]) -> String {
    format!("stripped {}", blake3::hash(&strip_labels(content)).to_hex())
}

/// The index of the base whose single-base merge differs from Forklift's at the fewest paths, the
/// earliest listed on a tie; None when no base was listed.
pub fn fewest_differing(counts: &[usize]) -> Option<usize> {
    counts.iter().enumerate().min_by_key(|(index, count)| (**count, *index)).map(|(index, _)| index)
}

/// Whether a consolidate that came to `outcome` left a working tree other than what it recorded,
/// by the stocktake it was followed with: any staged or unstaged change. Only a merge or a
/// fast-forward records a tree to compare with.
pub fn record_mismatch(outcome: &str, stocktake: &serde_json::Value) -> bool {
    let changed = |field: &str| stocktake["data"][field].as_u64() != Some(0);
    matches!(outcome, "merged" | "fast_forward") && (changed("staged_count") || changed("unstaged_count"))
}

/// What a row came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    /// No differing path, both clean, and the tree is the committed one.
    Agree,
    /// No differing path, both clean, and the committed tree differs: the merge was edited.
    HandEdited,
    /// No differing path, and both conflicted.
    AgreeConflicted,
    /// Gap paths only.
    Gap,
    /// At least one divergence.
    Divergent,
    /// No merge base, and neither tool merged.
    BothRefused,
    /// Forklift refused a merge git made.
    ForkliftRefused,
    /// Any other failure, in either tool or in the harness.
    Error,
}

impl Class {
    pub const ALL: [Class; 8] = [Class::Agree, Class::HandEdited, Class::AgreeConflicted, Class::Gap, Class::Divergent, Class::BothRefused, Class::ForkliftRefused, Class::Error];

    pub fn label(self) -> &'static str {
        match self {
            Class::Agree => "agree",
            Class::HandEdited => "hand-edited",
            Class::AgreeConflicted => "agree-conflicted",
            Class::Gap => "gap",
            Class::Divergent => "divergent",
            Class::BothRefused => "both-refused",
            Class::ForkliftRefused => "forklift-refused",
            Class::Error => "error",
        }
    }
}

/// The class of a row both tools merged, from its differing paths.
pub fn class_of(forklift_conflicted: bool, tree_is_committed: bool, paths: &[PathRow]) -> Class {
    if paths.iter().any(|path| path.difference.is_divergence()) {
        Class::Divergent
    } else if !paths.is_empty() {
        Class::Gap
    } else if forklift_conflicted {
        Class::AgreeConflicted
    } else if tree_is_committed {
        Class::Agree
    } else {
        Class::HandEdited
    }
}

/// One differing path of a row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PathRow {
    /// Empty for a record-mismatch.
    pub path: String,
    /// Forklift's file at the path, conflict markers included: its blob id, or "absent". Empty for
    /// a record-mismatch.
    pub blob: String,
    #[serde(flatten)]
    pub difference: Difference,
    /// On a forklift-clean-git-conflicted path, whether Forklift's file is the committed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equals_committed: Option<bool>,
}

/// The first hunk of a unified diff between Forklift's file and a reference's, or a note where
/// there is nothing to diff.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Hunk {
    pub path: String,
    /// g0, g, or committed.
    pub against: String,
    pub text: String,
}

/// One replayed merge.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Row {
    /// The merge's original commit id, before the rewrite.
    pub merge: String,
    pub class: Class,
    /// How many merge bases git lists for the two parents.
    pub bases: usize,
    /// On a row with several bases, the one G0 merged against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub single_base: Option<String>,
    /// What Forklift's consolidate came to: its outcome, or "refused".
    pub forklift: String,
    pub forklift_conflicts: Vec<String>,
    /// G's conflicted paths; None when git did not merge.
    pub git_conflicts: Option<Vec<String>>,
    pub paths: Vec<PathRow>,
    pub hunks: Vec<Hunk>,
    /// Differing paths beyond the ones given a hunk.
    pub hunks_left_out: usize,
    /// Why a row is an error, or what each tool said when it refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// On an error row whose comparison failed, what getting the warehouse out of the consolidate
    /// said when that failed too. A row entry is matched against `reason` alone, so an entry for a
    /// refused abort does not cover a comparison that starts failing on the same merge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clearing: Option<String>,
    pub millis: u64,
}

impl Row {
    fn new(merge: &str) -> Row {
        Row {
            merge: merge.to_string(), class: Class::Error, bases: 0, single_base: None, forklift: String::new(), forklift_conflicts: Vec::new(),
            git_conflicts: None, paths: Vec::new(), hunks: Vec::new(), hunks_left_out: 0, reason: None, clearing: None,
            millis: 0,
        }
    }
}

/// One entry of replay-tickets.json: a cause entry covers every gap of its cause; a path entry
/// covers one divergent path of one merge, while Forklift's file there stays the one it names; a
/// row entry covers one forklift-refused or error row of one merge, while the row's reason still
/// holds the text it names.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Ticket {
    Cause(CauseTicket),
    Path(PathTicket),
    Row(RowTicket),
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CauseTicket {
    pub cause: Cause,
    pub ticket: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathTicket {
    /// The merge's original 40-hex commit id.
    pub merge: String,
    pub path: String,
    pub kind: Kind,
    /// Forklift's file at the path: its 40-hex blob id, or "absent".
    pub blob: String,
    pub ticket: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowTicket {
    /// The merge's original 40-hex commit id.
    pub merge: String,
    /// `forklift-refused` or `error`.
    pub kind: Kind,
    /// Text the row's reason contains, so the entry stops matching when the row fails for
    /// another reason.
    pub reason: String,
    pub ticket: String,
}

fn hex40(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Refuses an entry no row could ever match: an entry without a 40-hex merge, a path entry of a
/// row-level kind, a record-mismatch with a path or a blob, another path kind without a path or
/// with a blob that is neither 40-hex nor "absent", a row entry of a path kind or with no reason,
/// and any ticket that is not a FORK ticket.
pub fn check_tickets(tickets: &[Ticket]) -> Result<(), String> {
    for ticket in tickets {
        let (name, problem) = match ticket {
            Ticket::Cause(entry) => (&entry.ticket, None),
            Ticket::Path(entry) => (&entry.ticket, if !hex40(&entry.merge) {
                Some(format!("merge {:?} is not a 40-hex commit id", entry.merge))
            } else if entry.kind.row_level() {
                Some(format!("{} names a row, so it takes a row entry: merge, kind, reason and ticket", entry.kind.label()))
            } else if entry.kind == Kind::RecordMismatch {
                (!entry.path.is_empty() || !entry.blob.is_empty()).then(|| "record-mismatch is about a row's whole tree, so its path and blob are empty".to_string())
            } else if entry.path.is_empty() {
                Some(format!("{} names a path, and the entry gives none", entry.kind.label()))
            } else if !hex40(&entry.blob) && entry.blob != PathState::ABSENT {
                Some(format!("blob {:?} is neither a 40-hex blob id nor \"absent\"", entry.blob))
            } else {
                None
            }),
            Ticket::Row(entry) => (&entry.ticket, if !hex40(&entry.merge) {
                Some(format!("merge {:?} is not a 40-hex commit id", entry.merge))
            } else if !entry.kind.row_level() {
                Some(format!("{} names a path, so it takes a path entry: merge, path, kind, blob and ticket", entry.kind.label()))
            } else if entry.reason.is_empty() {
                Some("a row entry names text its row's reason contains, and this one gives none".to_string())
            } else {
                None
            }),
        };
        let numbered = name.strip_prefix("FORK-").is_some_and(|number| !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()));
        if !numbered {
            return Err(format!("replay-tickets.json: {:?} is not a FORK ticket", name));
        }
        if let Some(problem) = problem {
            return Err(format!("replay-tickets.json: an entry under {}: {}", name, problem));
        }
    }
    Ok(())
}

/// The tickets this build was shipped with, crates/proof-load/replay-tickets.json, checked.
pub fn tickets() -> Result<Vec<Ticket>, String> {
    let tickets: Vec<Ticket> = serde_json::from_str(include_str!("../replay-tickets.json")).map_err(|error| format!("replay-tickets.json: {}", error))?;
    check_tickets(&tickets)?;
    Ok(tickets)
}

/// What failed the run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Failure {
    pub merge: String,
    pub path: String,
    pub kind: String,
    pub blob: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Verdict {
    pub failures: Vec<Failure>,
    /// Entries no row of this run matched: their case may be fixed, or this run's sample may not
    /// have met it. They never fail the run.
    pub closable: Vec<Ticket>,
}

/// Judges `rows` against `tickets`. A divergent path fails without a path entry naming its merge,
/// path, kind and Forklift's blob; a gap fails without a cause entry for its cause; a
/// forklift-refused or error row fails without a row entry naming its merge and kind and text its
/// reason contains, and its paths, where it has any, are judged as well. Nothing else fails.
pub fn judge(rows: &[Row], tickets: &[Ticket]) -> Verdict {
    let mut matched = vec![false; tickets.len()];
    let mut failures = Vec::new();
    let mut find = |found: &dyn Fn(&Ticket) -> bool| -> bool {
        let mut any = false;
        for (index, ticket) in tickets.iter().enumerate() {
            if found(ticket) {
                matched[index] = true;
                any = true;
            }
        }
        any
    };
    let path_entry = |merge: &str, path: &str, kind: Kind, blob: &str| {
        let (merge, path, blob) = (merge.to_string(), path.to_string(), blob.to_string());
        move |ticket: &Ticket| matches!(ticket, Ticket::Path(entry) if entry.merge == merge && entry.path == path && entry.kind == kind && entry.blob == blob)
    };
    for row in rows {
        let row_kind = match row.class {
            Class::ForkliftRefused => Some(Kind::ForkliftRefused),
            Class::Error => Some(Kind::Error),
            _ => None,
        };
        if let Some(kind) = row_kind {
            let reason = row.reason.clone().unwrap_or_default();
            if !find(&|ticket| matches!(ticket, Ticket::Row(entry) if entry.merge == row.merge && entry.kind == kind && reason.contains(&entry.reason))) {
                failures.push(Failure {
                    merge: row.merge.clone(), path: String::new(), kind: kind.label().to_string(), blob: String::new(),
                    reason: format!("a {} row no entry names with this merge, kind and reason", kind.label()),
                });
            }
        }
        for path in &row.paths {
            let (kind, found) = match path.difference {
                Difference::Divergence { subtype } => (subtype.label(), find(&path_entry(&row.merge, &path.path, subtype, &path.blob))),
                Difference::RenameGap { .. } => ("rename-gap", find(&|ticket| matches!(ticket, Ticket::Cause(entry) if entry.cause == Cause::Rename))),
                Difference::MultiBaseGap { .. } => ("multi-base-gap", find(&|ticket| matches!(ticket, Ticket::Cause(entry) if entry.cause == Cause::MultiBase))),
            };
            if !found {
                let reason = if path.difference.is_divergence() {
                    "a divergence no entry names with this merge, path, kind and blob".to_string()
                } else {
                    format!("a {} no cause entry covers", kind)
                };
                failures.push(Failure { merge: row.merge.clone(), path: path.path.clone(), kind: kind.to_string(), blob: path.blob.clone(), reason });
            }
        }
    }
    let closable = tickets.iter().zip(&matched).filter(|(_, matched)| !**matched).map(|(ticket, _)| ticket.clone()).collect();
    Verdict { failures, closable }
}

/// The merges to replay out of every two-parent merge in `rev-list --merges` order: shuffled by
/// `seed` when one is given, a Fisher-Yates shuffle driven by splitmix64, then the first `sample`.
pub fn sampled(mut merges: Vec<String>, sample: Option<usize>, seed: Option<u64>) -> Vec<String> {
    if let Some(seed) = seed {
        let mut state = seed;
        for index in (1..merges.len()).rev() {
            let other = (crate::history::splitmix(&mut state) % (index as u64 + 1)) as usize;
            merges.swap(index, other);
        }
    }
    if let Some(sample) = sample {
        merges.truncate(sample);
    }
    merges
}

/// The merges of the history.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Merges {
    pub in_history: usize,
    pub two_parent: usize,
    /// Merges of more than two parents, which are not replayed.
    pub more_parents: usize,
    pub replayed: usize,
}

/// How the mirror's history was rewritten before anything was merged.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Rewrite {
    /// Gitlink entries dropped: import skips them, so they would make every tree holding one
    /// differ from git's for a reason that is not the merge.
    pub gitlinks_dropped: usize,
    /// .forkliftignore changes dropped while `forkliftignore_ticket` is open.
    pub forkliftignore_dropped: usize,
    pub forkliftignore_ticket: String,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Header {
    pub source: String,
    /// The ref as given, and the commit it resolved to.
    pub reference: String,
    pub commit: String,
    pub sample: Option<usize>,
    pub seed: Option<u64>,
    pub merges: Merges,
    pub rewrite: Rewrite,
    pub tools: BTreeMap<String, String>,
    pub platform: Platform,
    pub os: String,
    pub import_seconds: f64,
    pub replay_seconds: f64,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ReplayReport {
    pub format: u32,
    pub header: Header,
    /// How many rows came to each class.
    pub classes: BTreeMap<String, usize>,
    pub rows: Vec<Row>,
    pub verdict: Verdict,
}

impl ReplayReport {
    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| format!("{{\"error\":\"{}\"}}", error))
    }

    pub fn failed(&self) -> bool {
        !self.verdict.failures.is_empty()
    }

    pub fn markdown(&self) -> String {
        let header = &self.header;
        let mut out = String::from("# Proof Load merge replay\n\n## Header\n\n");
        let _ = writeln!(out, "- format: {}", self.format);
        let _ = writeln!(out, "- source: {}", header.source);
        let _ = writeln!(out, "- ref: {} ({})", header.reference, header.commit);
        let _ = writeln!(out, "- sample: {}", header.sample.map_or("all".to_string(), |sample| sample.to_string()));
        let _ = writeln!(out, "- seed: {}", header.seed.map_or("none, in rev-list order".to_string(), |seed| seed.to_string()));
        let merges = &header.merges;
        let _ = writeln!(out, "- merges: {} in the history, {} of two parents, {} of more parents not replayed; {} replayed",
            merges.in_history, merges.two_parent, merges.more_parents, merges.replayed);
        let rewrite = &header.rewrite;
        let _ = writeln!(out, "- rewrite: {} gitlink entries dropped, since import skips them; {} .forkliftignore changes dropped while {} is open",
            rewrite.gitlinks_dropped, rewrite.forkliftignore_dropped, rewrite.forkliftignore_ticket);
        for (name, described) in &header.tools {
            let _ = writeln!(out, "- {}: {}", name, described);
        }
        let _ = writeln!(out, "- platform: {}; {}", header.platform.label(), header.os);
        let _ = writeln!(out, "- import {:.1} s, replay {:.1} s", header.import_seconds, header.replay_seconds);

        out.push_str("\n## Classes\n\n| class | rows |\n|---|---|\n");
        for class in Class::ALL {
            let _ = writeln!(out, "| {} | {} |", class.label(), self.classes.get(class.label()).copied().unwrap_or(0));
        }

        out.push_str("\n## Verdict\n\n");
        if self.verdict.failures.is_empty() {
            out.push_str("Passed: every divergence, gap and refused or error row has its entry.\n");
        } else {
            let _ = writeln!(out, "Failed: {} without an entry.\n\n| merge | path | kind | blob | why |\n|---|---|---|---|---|", self.verdict.failures.len());
            for failure in &self.verdict.failures {
                let _ = writeln!(out, "| {} | {} | {} | {} | {} |", failure.merge, failure.path, failure.kind, failure.blob, failure.reason);
            }
        }
        if !self.verdict.closable.is_empty() {
            out.push_str("\nEntries no row of this run matched, possibly closable:\n\n");
            for ticket in &self.verdict.closable {
                let _ = match ticket {
                    Ticket::Cause(entry) => writeln!(out, "- cause {}, {}", entry.cause.label(), entry.ticket),
                    Ticket::Path(entry) => writeln!(out, "- {} {} {} {}, {}", entry.merge, entry.path, entry.kind.label(), entry.blob, entry.ticket),
                    Ticket::Row(entry) => writeln!(out, "- {} {} {:?}, {}", entry.merge, entry.kind.label(), entry.reason, entry.ticket),
                };
            }
        }

        let shown: Vec<&Row> = self.rows.iter().filter(|row| row.class != Class::Agree).collect();
        if !shown.is_empty() {
            out.push_str("\n## Rows other than agree\n");
        }
        for row in shown {
            let _ = writeln!(out, "\n### {} — {}\n", row.merge, row.class.label());
            let _ = writeln!(out, "- bases: {}{}", row.bases, row.single_base.as_ref().map_or(String::new(), |base| format!(", G0 against {}", base)));
            let _ = writeln!(out, "- forklift: {}{}", row.forklift, if row.forklift_conflicts.is_empty() { String::new() } else { format!(", conflicted on {}", row.forklift_conflicts.join(", ")) });
            let _ = writeln!(out, "- git: {}", match &row.git_conflicts {
                None => "did not merge".to_string(),
                Some(conflicts) if conflicts.is_empty() => "clean".to_string(),
                Some(conflicts) => format!("conflicted on {}", conflicts.join(", ")),
            });
            if let Some(reason) = &row.reason {
                let _ = writeln!(out, "- reason: {}", reason);
            }
            if !row.paths.is_empty() {
                out.push_str("\n| path | kind | direction or subtype | blob |\n|---|---|---|---|\n");
                for path in &row.paths {
                    let (kind, detail) = match path.difference {
                        Difference::Divergence { subtype } => ("divergence", subtype.label().to_string()),
                        Difference::RenameGap { direction } => ("rename gap", format!("{:?}", direction)),
                        Difference::MultiBaseGap { direction } => ("multi-base gap", format!("{:?}", direction)),
                    };
                    let committed = match path.equals_committed {
                        Some(true) => ", Forklift's file is the committed one",
                        Some(false) => ", Forklift's file is not the committed one",
                        None => "",
                    };
                    let _ = writeln!(out, "| {} | {} | {}{} | {} |", path.path, kind, detail, committed, path.blob);
                }
            }
            for hunk in &row.hunks {
                let _ = writeln!(out, "\n{} against {}:\n\n````diff\n{}\n````", hunk.path, hunk.against, hunk.text.trim_end());
            }
            if row.hunks_left_out > 0 {
                let _ = writeln!(out, "\n{} more differing paths have no hunk here.", row.hunks_left_out);
            }
        }
        out
    }
}

/// The first hunk of `diff`'s unified diff, cut at [`HUNK_LINES`] lines; the whole output when it
/// has no hunk, as for a binary file.
pub fn first_hunk(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    let Some(start) = lines.iter().position(|line| line.starts_with("@@")) else {
        return diff.trim_end().to_string();
    };
    let end = lines[start + 1..].iter().position(|line| line.starts_with("@@")).map_or(lines.len(), |offset| start + 1 + offset);
    let mut hunk: Vec<&str> = lines[start..end].to_vec();
    let cut = hunk.len() > HUNK_LINES;
    hunk.truncate(HUNK_LINES);
    let mut text = hunk.join("\n");
    if cut {
        let _ = write!(text, "\n… cut at {} lines", HUNK_LINES);
    }
    text
}

/// The pallet a merge's part is imported as: `o` its first parent, `t` its second, `m` the merge.
fn branch(merge: &str, part: &str) -> String {
    format!("m-{}-{}", merge, part)
}

/// Drops from a `git fast-export --no-data` stream every gitlink entry and every change to a
/// .forkliftignore file, counting each; everything else, commit messages included, is kept byte
/// for byte.
pub fn rewritten(stream: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let (mut kept, mut gitlinks, mut ignores) = (Vec::with_capacity(stream.len()), 0, 0);
    let mut at = 0;
    while at < stream.len() {
        let end = stream[at..].iter().position(|byte| *byte == b'\n').map_or(stream.len(), |offset| at + offset);
        let line = &stream[at..end];
        let next = (end + 1).min(stream.len());
        if let Some(length) = line.strip_prefix(b"data ") {
            let length: usize = std::str::from_utf8(length).ok().and_then(|length| length.parse().ok())
                .ok_or_else(|| format!("fast-export printed a data line this harness does not read: {:?}", String::from_utf8_lossy(line)))?;
            let data_end = next.checked_add(length).filter(|data_end| *data_end <= stream.len())
                .ok_or_else(|| "fast-export's stream ends inside a data block".to_string())?;
            kept.extend_from_slice(&stream[at..data_end]);
            at = data_end;
            continue;
        }
        let path = if let Some(rest) = line.strip_prefix(b"M ") {
            if rest.starts_with(b"160000 ") {
                gitlinks += 1;
                at = next;
                continue;
            }
            rest.splitn(3, |byte| *byte == b' ').nth(2)
        } else {
            line.strip_prefix(b"D ")
        };
        if let Some(path) = path {
            let path = path.strip_prefix(b"\"").and_then(|path| path.strip_suffix(b"\"")).unwrap_or(path);
            if path == b".forkliftignore" || path.ends_with(b"/.forkliftignore") {
                ignores += 1;
                at = next;
                continue;
            }
        }
        kept.extend_from_slice(&stream[at..next]);
        at = next;
    }
    Ok((kept, gitlinks, ignores))
}

/// A merge result: the paths it conflicted on, and its tree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Merged {
    conflicted: BTreeSet<String>,
    tree: String,
}

/// The tools of one run and where they work.
struct Tools<'a> {
    forklift: PathBuf,
    git: PathBuf,
    mirror: PathBuf,
    warehouse: PathBuf,
    /// The index Forklift's tree is hashed through, made anew for each hash.
    index: PathBuf,
    scratch: &'a Scratch,
    environment: &'a Environment,
}

fn said(outcome: &Outcome) -> String {
    let stream = if outcome.stderr.iter().all(u8::is_ascii_whitespace) { &outcome.stdout } else { &outcome.stderr };
    let text = String::from_utf8_lossy(&stream[..stream.len().min(4096)]).trim().to_string();
    format!("`{}` exited {}: {}", outcome.arguments.join(" "), outcome.exit.map_or("on a signal".to_string(), |code| code.to_string()), text)
}

impl Tools<'_> {
    fn run(&self, program: &Path, arguments: &[&str], directory: &Path, environment: &Environment, limit: Duration, input: Option<&Path>) -> Result<Outcome, String> {
        let invocation = Invocation {
            program: program.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
            directory: directory.to_path_buf(), limit,
        };
        match input {
            Some(input) => tool::run_with_input(&invocation, input, self.scratch, environment),
            None => tool::run(&invocation, self.scratch, environment),
        }.map_err(|error| error.to_string())
    }

    /// git in the mirror, with attributes read from the empty tree, so no history's
    /// `.gitattributes` changes git's merges or diffs, as none changes Forklift's; any exit
    /// status.
    fn git(&self, arguments: &[&str]) -> Result<Outcome, String> {
        let mut full = vec!["--git-dir", self.mirror.to_str().ok_or("the mirror's path is not UTF-8")?, "--attr-source", EMPTY_TREE];
        full.extend_from_slice(arguments);
        self.run(&self.git, &full, &self.mirror, self.environment, STEP_LIMIT, None)
    }

    /// git in the mirror, refused unless it exits 0; its stdout.
    fn git_ok(&self, arguments: &[&str]) -> Result<Vec<u8>, String> {
        let outcome = self.git(arguments)?;
        if !outcome.succeeded() {
            return Err(said(&outcome));
        }
        Ok(outcome.stdout)
    }

    fn git_line(&self, arguments: &[&str]) -> Result<String, String> {
        Ok(String::from_utf8_lossy(&self.git_ok(arguments)?).trim().to_string())
    }

    fn forklift(&self, arguments: &[&str]) -> Result<Outcome, String> {
        self.run(&self.forklift, arguments, &self.warehouse, self.environment, STEP_LIMIT, None)
    }

    /// Puts each of `conflicted` back as `ours` has it, its mode included, or removes it where
    /// `ours` has none, then aborts the consolidate in progress. For an abort refused over the
    /// conflicted files' content, so one refused abort does not leave every later row unable to
    /// shift.
    fn settle(&self, ours: &str, conflicted: &BTreeSet<String>) -> Result<(), String> {
        for path in conflicted {
            let file = self.warehouse.join(path);
            let cannot = |error: std::io::Error| format!("cannot put {} back: {}", file.display(), error);
            let listed = self.git_ok(&["ls-tree", "-z", ours, "--", path])?;
            let listed = String::from_utf8_lossy(&listed).into_owned();
            let mode = listed.split(' ').next().filter(|mode| !mode.is_empty()).map(str::to_string);
            #[allow(clippy::disallowed_methods, reason = "a file of the harness's own warehouse, which nothing else holds open")]
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(cannot(error)),
            }
            let Some(mode) = mode else { continue };
            let content = self.git_ok(&["cat-file", "blob", &format!("{}:{}", ours, path)])?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).map_err(cannot)?;
            }
            match mode.as_str() {
                #[cfg(unix)]
                "120000" => std::os::unix::fs::symlink(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&content), &file).map_err(cannot)?,
                _ => std::fs::write(&file, &content).map_err(cannot)?,
            }
            #[cfg(unix)]
            if mode == "100755" {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).map_err(cannot)?;
            }
        }
        let aborted = self.forklift(&["--json", "abort", "--confirm"])?;
        if !aborted.succeeded() {
            return Err(said(&aborted));
        }
        Ok(())
    }

    /// Forklift's working tree as a git tree id: its files' bytes as they are, hashed through an
    /// index built from nothing, so no stat cache from an earlier hash can stand in for a file's
    /// content, and with attributes read from the empty tree and line-ending conversion off, so a
    /// history's own .gitattributes does not rewrite what Forklift checked out before it is
    /// compared.
    fn forklift_tree(&self) -> Result<String, String> {
        #[allow(clippy::disallowed_methods, reason = "the harness's own scratch index, which nothing else holds open")]
        match std::fs::remove_file(&self.index) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {}", self.index.display(), error)),
        }
        let mut environment = self.environment.clone();
        environment.variables.insert("GIT_INDEX_FILE".to_string(), self.index.display().to_string());
        environment.variables.insert("GIT_WORK_TREE".to_string(), self.warehouse.display().to_string());
        let mirror = self.mirror.display().to_string();
        let raw = ["--attr-source", EMPTY_TREE, "-c", "core.autocrlf=false", "-c", "core.safecrlf=false"];
        let mut arguments = vec!["--git-dir", mirror.as_str()];
        arguments.extend_from_slice(&raw);
        arguments.extend_from_slice(&["add", "-A", "-f", "--", ".", ":(exclude).forklift"]);
        let added = self.run(&self.git, &arguments, &self.warehouse, &environment, STEP_LIMIT, None)?;
        if !added.succeeded() {
            return Err(said(&added));
        }
        let written = self.run(&self.git, &["--git-dir", &mirror, "write-tree"], &self.warehouse, &environment, STEP_LIMIT, None)?;
        if !written.succeeded() {
            return Err(said(&written));
        }
        Ok(String::from_utf8_lossy(&written.stdout).trim().to_string())
    }

    /// git's merge of `ours` and `theirs` with diff3 conflict markers and `options`; Ok(Err) with
    /// what git said when it refused to merge.
    fn merge_tree(&self, ours: &str, theirs: &str, options: &[&str]) -> Result<Result<Merged, String>, String> {
        let mut arguments = vec!["-c", "merge.conflictStyle=diff3", "merge-tree", "--write-tree", "--name-only", "-z"];
        arguments.extend_from_slice(options);
        arguments.extend_from_slice(&[ours, theirs]);
        let outcome = self.git(&arguments)?;
        if !matches!(outcome.exit, Some(0) | Some(1)) {
            return Ok(Err(said(&outcome)));
        }
        let text = String::from_utf8_lossy(&outcome.stdout).into_owned();
        let mut fields = text.split('\0');
        let tree = fields.next().unwrap_or_default().trim().to_string();
        if !hex40(&tree) {
            return Err(format!("git merge-tree printed no tree id: {:?}", &text[..text.len().min(200)]));
        }
        let conflicted = if outcome.exit == Some(1) { fields.take_while(|name| !name.is_empty()).map(str::to_string).collect() } else { BTreeSet::new() };
        Ok(Ok(Merged { conflicted, tree }))
    }
}

/// Each tree's files, read once per row.
struct Trees<'a, 'b> {
    tools: &'a Tools<'b>,
    listed: BTreeMap<String, BTreeMap<String, (String, String)>>,
}

impl Trees<'_, '_> {
    /// The (mode, blob) of every file in `tree`.
    fn files(&mut self, tree: &str) -> Result<&BTreeMap<String, (String, String)>, String> {
        if !self.listed.contains_key(tree) {
            let listing = self.tools.git_ok(&["ls-tree", "-r", "-z", "--full-tree", tree])?;
            let mut files = BTreeMap::new();
            for entry in listing.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()) {
                let entry = String::from_utf8_lossy(entry);
                let (meta, path) = entry.split_once('\t').ok_or_else(|| format!("git ls-tree printed {:?}", entry))?;
                let mut meta = meta.split(' ');
                let (mode, _, blob) = (meta.next().unwrap_or_default(), meta.next(), meta.next().unwrap_or_default());
                files.insert(path.to_string(), (mode.to_string(), blob.to_string()));
            }
            self.listed.insert(tree.to_string(), files);
        }
        Ok(&self.listed[tree])
    }

    fn blob(&mut self, tree: &str, path: &str) -> Result<Option<String>, String> {
        Ok(self.files(tree)?.get(path).map(|(_, blob)| blob.clone()))
    }

    fn state(&mut self, merged: &Merged, path: &str) -> Result<PathState, String> {
        let conflicted = merged.conflicted.contains(path);
        let value = match self.files(&merged.tree)?.get(path).cloned() {
            None => PathState::ABSENT.to_string(),
            Some((mode, blob)) if !conflicted => format!("{} {}", mode, blob),
            Some((_, blob)) => conflicted_value(&self.tools.git_ok(&["cat-file", "blob", &blob])?),
        };
        Ok(PathState { conflicted, value })
    }

    /// Every path that can differ between two results: those their trees differ on and those
    /// either conflicted on.
    fn candidates(&mut self, a: &Merged, b: &Merged) -> Result<BTreeSet<String>, String> {
        let mut paths: BTreeSet<String> = a.conflicted.union(&b.conflicted).cloned().collect();
        if a.tree != b.tree {
            let names = self.tools.git_ok(&["diff-tree", "-r", "--name-only", "-z", &a.tree, &b.tree])?;
            paths.extend(names.split(|byte| *byte == 0).filter(|name| !name.is_empty()).map(|name| String::from_utf8_lossy(name).into_owned()));
        }
        Ok(paths)
    }

    fn differing(&mut self, a: &Merged, b: &Merged) -> Result<usize, String> {
        let mut count = 0;
        for path in self.candidates(a, b)? {
            if self.state(a, &path)?.differs(&self.state(b, &path)?) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// The first hunk of the diff from blob `from` to blob `to`, or a note where either is absent.
    /// Two conflicted files are diffed with their marker labels stripped, so the hunk shows the
    /// line they differ on rather than the first label.
    fn hunk(&mut self, from: Option<&str>, to: Option<&str>, both_conflicted: bool) -> Result<String, String> {
        match (from, to) {
            (None, None) => Ok("absent in both".to_string()),
            (None, Some(_)) => Ok("absent in Forklift's tree".to_string()),
            (Some(_), None) => Ok("absent in the reference's tree".to_string()),
            (Some(from), Some(to)) if from == to => Ok("the files are the same; only whether the path is conflicted differs".to_string()),
            (Some(from), Some(to)) if both_conflicted => {
                let mut files = Vec::new();
                for (name, blob) in [("replay-hunk-forklift", from), ("replay-hunk-reference", to)] {
                    let file = self.tools.scratch.tmp().join(name);
                    std::fs::write(&file, strip_labels(&self.tools.git_ok(&["cat-file", "blob", blob])?)).map_err(|error| format!("cannot write {}: {}", file.display(), error))?;
                    files.push(file.display().to_string());
                }
                let diff = self.tools.git(&["diff", "--no-index", "--no-color", "--no-ext-diff", "-U3", "--", &files[0], &files[1]])?;
                if !matches!(diff.exit, Some(0) | Some(1)) {
                    return Err(said(&diff));
                }
                Ok(first_hunk(&String::from_utf8_lossy(&diff.stdout)))
            }
            (Some(from), Some(to)) => {
                let diff = self.tools.git_ok(&["diff", "--no-color", "--no-ext-diff", "-U3", from, to])?;
                Ok(first_hunk(&String::from_utf8_lossy(&diff)))
            }
        }
    }
}

/// What a consolidate's report says: Ok(Ok(None)) a merge with no conflict, Ok(Ok(Some)) the
/// paths it conflicted on, Ok(Err) a refusal; Err for a report the harness does not read, so an
/// outcome it does not know, or a conflict that is not a path, is an error row and never a
/// refusal or a dropped path.
fn consolidate_said(succeeded: bool, document: &serde_json::Value) -> Result<Result<Option<BTreeSet<String>>, ()>, String> {
    if !succeeded {
        return Ok(Err(()));
    }
    match document["data"]["outcome"].as_str() {
        Some("merged" | "fast_forward" | "up_to_date") => Ok(Ok(None)),
        Some("conflicts") => {
            let listed = match &document["data"]["conflicts"] {
                serde_json::Value::Array(listed) => listed,
                other => return Err(format!("the consolidate's conflicts are {}, not a list of paths", other)),
            };
            let mut conflicted = BTreeSet::new();
            for path in listed {
                let serde_json::Value::String(path) = path else {
                    return Err(format!("the consolidate listed a conflict that is not a path: {}", path));
                };
                conflicted.insert(path.clone());
            }
            Ok(Ok(Some(conflicted)))
        }
        _ => Err(format!("the consolidate succeeded with outcome {}, which the harness does not read", document["data"]["outcome"])),
    }
}

/// What a replay leaves to clear once its row is compared.
enum Leftover {
    /// No consolidate is in progress.
    Nothing,
    /// A consolidate conflicted on these paths and is still in progress; the first parent's id.
    Conflicted { ours: String, conflicted: BTreeSet<String> },
    /// A consolidate was refused or reported what the harness does not read, and may have left a
    /// merge in progress.
    Unknown,
}

/// Replays one merge, by its original id, into `row`: compares it, then gets the warehouse out of
/// the consolidate. Err is an error row's reason: the comparison's failure where it failed, else
/// the clean-up's. The paths the comparison found stay on the row and are judged with it.
fn replay(tools: &Tools, merge: &str, row: &mut Row) -> Result<(), String> {
    let mut leftover = Leftover::Nothing;
    let compared = compare(tools, merge, row, &mut leftover);
    let cleared = match leftover {
        Leftover::Nothing => Ok(()),
        Leftover::Conflicted { ours, conflicted } => {
            let aborted = tools.forklift(&["--json", "abort", "--confirm"])?;
            if aborted.succeeded() {
                Ok(())
            } else {
                let refused = format!("aborting the conflicted consolidate failed: {}", said(&aborted));
                // The row is an error either way. Its conflicted files are put back as the first
                // parent has them, the way out the refusal names, so the next row starts from a
                // warehouse with no consolidate in progress.
                Err(match settled(tools, &ours, &conflicted) {
                    Ok(()) => refused,
                    Err(unsettled) => format!("{}; putting the conflicted files back did not let it abort: {}", refused, unsettled),
                })
            }
        }
        // Whether anything is in progress is unknown; the probe after the row says so if this one
        // leaves it behind.
        Leftover::Unknown => { let _ = tools.forklift(&["--json", "abort", "--confirm"]); Ok(()) }
    };
    outcome(row, compared, cleared)
}

/// A row's result from its comparison's and its clean-up's: the comparison's failure is the
/// reason, and a clean-up failure after it goes to `clearing`, apart from it.
fn outcome(row: &mut Row, compared: Result<(), String>, cleared: Result<(), String>) -> Result<(), String> {
    match (compared, cleared) {
        (Ok(()), cleared) => cleared,
        (Err(reason), Ok(())) => Err(reason),
        (Err(reason), Err(cleared)) => {
            row.clearing = Some(cleared);
            Err(reason)
        }
    }
}

/// Whether a row of `class` is followed by a probe that the warehouse can still shift: one whose
/// consolidate was refused, or which failed, may have left something in progress that the
/// abort after it did not clear.
fn probed(class: Class) -> bool {
    matches!(class, Class::ForkliftRefused | Class::BothRefused | Class::Error)
}

/// The settle step, or a stand-in that leaves the warehouse as the refused abort did, for the test
/// showing the run stops on a warehouse it cannot shift.
fn settled(tools: &Tools, ours: &str, conflicted: &BTreeSet<String>) -> Result<(), String> {
    #[cfg(test)]
    if tests::SKIP_SETTLE.with(std::cell::Cell::get) {
        return Err("settling skipped by the test".to_string());
    }
    tools.settle(ours, conflicted)
}

/// Compares one merge's replay with git's, into `row`, recording in `leftover` what the
/// consolidate left in progress.
fn compare(tools: &Tools, merge: &str, row: &mut Row, leftover: &mut Leftover) -> Result<(), String> {
    let (ours, theirs, merged) = (branch(merge, "o"), branch(merge, "t"), branch(merge, "m"));
    let our_id = tools.git_line(&["rev-parse", "--verify", &format!("refs/heads/{}", ours)])?;
    let their_id = tools.git_line(&["rev-parse", "--verify", &format!("refs/heads/{}", theirs)])?;
    let merge_id = tools.git_line(&["rev-parse", "--verify", &format!("refs/heads/{}", merged)])?;
    let parents = tools.git_line(&["rev-list", "--parents", "-n", "1", &merge_id])?;
    let expected = format!("{} {} {}", merge_id, our_id, their_id);
    if parents != expected {
        return Err(format!("the rewritten merge {} has parents {:?}, not its two parents {} and {}", merge_id, parents, our_id, their_id));
    }
    let committed = tools.git_line(&["rev-parse", &format!("{}^{{tree}}", merge_id)])?;
    let first_tree = tools.git_line(&["rev-parse", &format!("{}^{{tree}}", our_id)])?;
    let bases = tools.git(&["merge-base", "--all", &our_id, &their_id])?;
    let bases: Vec<String> = match bases.exit {
        Some(0) => String::from_utf8_lossy(&bases.stdout).split_whitespace().map(str::to_string).collect(),
        Some(1) => Vec::new(),
        _ => return Err(said(&bases)),
    };
    row.bases = bases.len();
    let g = tools.merge_tree(&our_id, &their_id, &[])?;
    let g1 = tools.merge_tree(&our_id, &their_id, &["-X", "no-renames"])?;

    let shifted = tools.forklift(&["--json", "shift", &ours])?;
    if !shifted.succeeded() {
        return Err(format!("shifting to the first parent failed: {}", said(&shifted)));
    }
    let checked_out = tools.forklift_tree()?;
    if checked_out != first_tree {
        return Err(format!("the first parent's checkout is tree {}, not its git tree {}", checked_out, first_tree));
    }
    let consolidated = tools.forklift(&["--json", "consolidate", &theirs])?;
    *leftover = Leftover::Unknown;
    let document = crate::runner::document(&consolidated.stdout);
    let outcome = document["data"]["outcome"].as_str().unwrap_or_default().to_string();
    let forklift: Result<Merged, String> = match consolidate_said(consolidated.succeeded(), &document)? {
        Ok(None) => {
            *leftover = Leftover::Nothing;
            Ok(Merged { conflicted: BTreeSet::new(), tree: tools.forklift_tree()? })
        }
        Ok(Some(conflicted)) => {
            *leftover = Leftover::Conflicted { ours: our_id.clone(), conflicted: conflicted.clone() };
            Ok(Merged { conflicted, tree: tools.forklift_tree()? })
        }
        Err(()) => Err(match &consolidated.failure {
            Some(failure) => format!("{}: {}", failure.code, failure.message),
            None => said(&consolidated),
        }),
    };
    row.forklift = if forklift.is_ok() { outcome.clone() } else { "refused".to_string() };
    let mut mismatch = false;
    if matches!(outcome.as_str(), "merged" | "fast_forward") && forklift.is_ok() {
        let stocktake = tools.forklift(&["--json", "stocktake"])?;
        if !stocktake.succeeded() {
            return Err(format!("stocktake after the consolidate failed: {}", said(&stocktake)));
        }
        mismatch = record_mismatch(&outcome, &crate::runner::document(&stocktake.stdout));
    }

    let (forklift, g) = match (forklift, g) {
        (Err(refused), Err(git_refused)) if bases.is_empty() => {
            row.class = Class::BothRefused;
            row.reason = Some(format!("no merge base; Forklift: {}; git: {}", refused, git_refused));
            return Ok(());
        }
        (Err(refused), Ok(g)) => {
            row.git_conflicts = Some(g.conflicted.iter().cloned().collect());
            row.class = Class::ForkliftRefused;
            row.reason = Some(refused);
            return Ok(());
        }
        (Err(refused), Err(git_refused)) => return Err(format!("both refused with a merge base; Forklift: {}; git: {}", refused, git_refused)),
        (Ok(_), Err(git_refused)) => return Err(format!("git did not merge where Forklift did: {}", git_refused)),
        (Ok(forklift), Ok(g)) => (forklift, g),
    };
    let g1 = g1.map_err(|refused| format!("git's merge without renames failed: {}", refused))?;
    row.forklift_conflicts = forklift.conflicted.iter().cloned().collect();
    row.git_conflicts = Some(g.conflicted.iter().cloned().collect());

    let mut trees = Trees { tools, listed: BTreeMap::new() };
    let g0 = if bases.len() > 1 {
        let mut runs = Vec::new();
        for base in &bases {
            let option = format!("--merge-base={}", base);
            let run = tools.merge_tree(&our_id, &their_id, &["-X", "no-renames", &option])?
                .map_err(|refused| format!("git's merge against base {} failed: {}", base, refused))?;
            runs.push(run);
        }
        let mut counts = Vec::new();
        for run in &runs {
            counts.push(trees.differing(&forklift, run)?);
        }
        let chosen = fewest_differing(&counts).expect("a row with several bases has runs");
        row.single_base = Some(bases[chosen].clone());
        runs.swap_remove(chosen)
    } else {
        g1.clone()
    };

    let mut candidates = trees.candidates(&forklift, &g)?;
    candidates.extend(trees.candidates(&forklift, &g0)?);
    candidates.extend(trees.candidates(&g, &g1)?);
    candidates.extend(trees.candidates(&g1, &g0)?);
    for path in candidates {
        let at = trees.state(&forklift, &path)?;
        let Some(difference) = difference(&at, &trees.state(&g, &path)?, &trees.state(&g1, &path)?, &trees.state(&g0, &path)?) else { continue };
        let blob = trees.blob(&forklift.tree, &path)?;
        let equals_committed = match difference {
            Difference::Divergence { subtype: Kind::ForkliftCleanGitConflicted } => Some(blob == trees.blob(&committed, &path)?),
            _ => None,
        };
        row.paths.push(PathRow { path, blob: blob.unwrap_or_else(|| PathState::ABSENT.to_string()), difference, equals_committed });
    }
    if mismatch {
        row.paths.push(PathRow { path: String::new(), blob: String::new(), difference: Difference::Divergence { subtype: Kind::RecordMismatch }, equals_committed: None });
    }
    row.class = class_of(!forklift.conflicted.is_empty(), forklift.tree == committed, &row.paths);

    let mut shown: Vec<&PathRow> = row.paths.iter().filter(|path| !path.path.is_empty()).collect();
    shown.sort_by_key(|path| !path.difference.is_divergence());
    row.hunks_left_out = shown.len().saturating_sub(HUNKS_PER_ROW);
    let mut hunks = Vec::new();
    for path in shown.into_iter().take(HUNKS_PER_ROW) {
        let ours = trees.blob(&forklift.tree, &path.path)?;
        let (against, reference) = if path.difference.is_divergence() { ("g0", &g0) } else { ("g", &g) };
        let theirs = trees.blob(&reference.tree, &path.path)?;
        let both_conflicted = forklift.conflicted.contains(&path.path) && reference.conflicted.contains(&path.path);
        hunks.push(Hunk { path: path.path.clone(), against: against.to_string(), text: trees.hunk(ours.as_deref(), theirs.as_deref(), both_conflicted)? });
        if path.difference == (Difference::Divergence { subtype: Kind::SilentWrong }) {
            let theirs = trees.blob(&committed, &path.path)?;
            hunks.push(Hunk { path: path.path.clone(), against: "committed".to_string(), text: trees.hunk(ours.as_deref(), theirs.as_deref(), false)? });
        }
    }
    row.hunks = hunks;
    Ok(())
}

/// Runs a replay and writes replay.md and then replay.json under `options.out`, replay.json last,
/// so a run that stopped leaves none. Refuses, writing neither, when it cannot build what it
/// replays, or when an error row leaves a warehouse that cannot shift.
pub fn run(options: &ReplayOptions, progress: &dyn Fn(&str)) -> Result<ReplayReport, String> {
    let tickets = tickets()?;
    if options.sample == Some(0) {
        return Err("--sample 0 replays no merge".to_string());
    }
    let absolute = |flag: &str, path: &Path| tool::absolute(path).map_err(|error| format!("{} {}: {}", flag, path.display(), error));
    let forklift = absolute("--forklift", &options.forklift)?;
    let git = absolute("--git", &options.git)?;
    let source = std::path::absolute(&options.source).map_err(|error| format!("--source {}: {}", options.source.display(), error))?;
    if !source.is_dir() {
        return Err(format!("--source {}: not a folder", source.display()));
    }
    let mut output = Output::claim(&options.out).map_err(|error| error.to_string())?;
    let scratch = output.scratch("replay").map_err(|error| error.to_string())?;
    let search_path = crate::scrub::search_path(Some(&git), std::env::var_os("PATH"));
    let environment = Environment::scrubbed(&scratch, search_path, &BTreeMap::new()).map_err(|error| error.to_string())?;

    let started = Instant::now();
    progress(&format!("mirroring the history at {}", source.display()));
    let (mirror, _) = crate::fidelity::take_history(&git, &source, &scratch, &environment, BUILD_LIMIT).copied
        .map_err(|reason| format!("the history could not be mirrored: {}", reason))?;
    let warehouse = scratch.work().join("warehouse");
    let tools = Tools { forklift: forklift.clone(), git: git.clone(), mirror: mirror.clone(), warehouse: warehouse.clone(), index: scratch.tmp().join("replay.index"), scratch: &scratch, environment: &environment };
    let build = |what: &str, outcome: Result<Outcome, String>| -> Result<Vec<u8>, String> {
        let outcome = outcome?;
        if !outcome.succeeded() {
            return Err(format!("{} failed: {}", what, said(&outcome)));
        }
        Ok(outcome.stdout)
    };
    let source_text = source.display().to_string();
    let resolved = build("resolving --ref", tools.run(&git, &["-C", &source_text, "rev-parse", "--verify", "--end-of-options", &format!("{}^{{commit}}", options.reference)], scratch.root(), &environment, BUILD_LIMIT, None))
        .map_err(|error| format!("--ref {}: {}", options.reference, error))?;
    let commit = String::from_utf8_lossy(&resolved).trim().to_string();
    let head = format!("refs/heads/{}", HEAD_BRANCH);
    build("fetching the ref's commit", tools.git(&["fetch", "-q", "--no-tags", &source_text, &format!("+{}:{}", commit, head)]))?;
    // The mirror holds only the branches the harness makes, so the import reads nothing else.
    let listed = build("listing the mirror's refs", tools.git(&["for-each-ref", "--format=%(refname)"]))?;
    let deletions: String = String::from_utf8_lossy(&listed).lines().filter(|name| *name != head).map(|name| format!("delete {}\n", name)).collect();
    let updates = scratch.tmp().join("replay-refs");
    std::fs::write(&updates, deletions).map_err(|error| format!("cannot write {}: {}", updates.display(), error))?;
    let mirror_text = mirror.display().to_string();
    build("deleting the mirror's other refs", tools.run(&git, &["--git-dir", &mirror_text, "update-ref", "--stdin"], &mirror, &environment, BUILD_LIMIT, Some(&updates)))?;
    build("making the ref's branch HEAD", tools.git(&["symbolic-ref", "HEAD", &head]))?;

    let listed = build("listing the merges", tools.git(&["rev-list", "--merges", "--parents", &commit]))?;
    let listed = String::from_utf8_lossy(&listed).into_owned();
    let mut two_parent = Vec::new();
    let mut parents_of = BTreeMap::new();
    let mut in_history = 0;
    for line in listed.lines() {
        in_history += 1;
        let ids: Vec<&str> = line.split_whitespace().collect();
        if ids.len() == 3 {
            two_parent.push(ids[0].to_string());
            parents_of.insert(ids[0].to_string(), (ids[1].to_string(), ids[2].to_string()));
        }
    }
    let merges = Merges { in_history, two_parent: two_parent.len(), more_parents: in_history - two_parent.len(), replayed: 0 };
    let chosen = sampled(two_parent, options.sample, options.seed);
    let mut creations = String::new();
    for merge in &chosen {
        let (ours, theirs) = &parents_of[merge];
        for (part, id) in [("o", ours), ("t", theirs), ("m", merge)] {
            let _ = writeln!(creations, "create refs/heads/{} {}", branch(merge, part), id);
        }
    }
    std::fs::write(&updates, creations).map_err(|error| format!("cannot write {}: {}", updates.display(), error))?;
    build("making the merges' branches", tools.run(&git, &["--git-dir", &mirror_text, "update-ref", "--stdin"], &mirror, &environment, BUILD_LIMIT, Some(&updates)))?;

    progress("rewriting the mirror's history");
    let exported = build("exporting the mirror", tools.run(&git, &["--git-dir", &mirror_text, "fast-export", "--no-data", "--reencode=no", "--signed-tags=strip", "--all"], &mirror, &environment, BUILD_LIMIT, None))?;
    // Gitlinks are dropped for good; .forkliftignore changes only until FORK-536 closes.
    let (stream, gitlinks_dropped, forkliftignore_dropped) = rewritten(&exported)?;
    drop(exported);
    let stream_path = scratch.tmp().join("replay.stream");
    std::fs::write(&stream_path, &stream).map_err(|error| format!("cannot write {}: {}", stream_path.display(), error))?;
    drop(stream);
    build("importing the rewritten history", tools.run(&git, &["--git-dir", &mirror_text, "fast-import", "--quiet", "--force"], &mirror, &environment, BUILD_LIMIT, Some(&stream_path)))?;
    #[allow(clippy::disallowed_methods, reason = "the harness's own stream, read in full by the import above")]
    let _ = std::fs::remove_file(&stream_path);
    let rewrite = Rewrite { gitlinks_dropped, forkliftignore_dropped, forkliftignore_ticket: IGNORE_TICKET.to_string() };

    progress(&format!("importing into Forklift: {} merges of {} two-parent ones", chosen.len(), merges.two_parent));
    std::fs::create_dir(&warehouse).map_err(|error| format!("cannot create {}: {}", warehouse.display(), error))?;
    let forklift_build = |arguments: &[&str]| build(&format!("forklift {}", arguments.join(" ")), tools.run(&forklift, arguments, &warehouse, &environment, BUILD_LIMIT, None));
    forklift_build(&["--json", "prepare"])?;
    let imported = forklift_build(&["--json", "git", "import", "--", &mirror_text])?;
    let current = crate::runner::document(&imported)["data"]["current"].as_str().map(str::to_string);
    if current.as_deref() != Some(HEAD_BRANCH) {
        return Err(format!("forklift's import made {:?} current, not {:?}, the mirror's HEAD", current, HEAD_BRANCH));
    }
    forklift_build(&["--json", "restore", "."])?;
    let import_seconds = started.elapsed().as_secs_f64();

    let replaying = Instant::now();
    let mut rows = Vec::new();
    for (index, merge) in chosen.iter().enumerate() {
        let started = Instant::now();
        let mut row = Row::new(merge);
        let failed = replay(&tools, merge, &mut row).err();
        row.millis = started.elapsed().as_millis() as u64;
        if let Some(reason) = failed {
            row.class = Class::Error;
            row.reason = Some(reason);
        }
        // A row whose consolidate was refused, or which failed, may have left the warehouse where
        // no shift works, and then every later row would be an error for this row's reason. The
        // run stops instead, naming it.
        if probed(row.class) {
            let probe = tools.forklift(&["--json", "shift", HEAD_BRANCH]).map_err(|error| format!("shifting after merge {}'s {} row: {}", merge, row.class.label(), error))?;
            if !probe.succeeded() {
                return Err(format!(
                    "the warehouse cannot shift after merge {}'s {} row, so no later row would replay: {}; the row: {}",
                    merge, row.class.label(), said(&probe), row.reason.as_deref().unwrap_or_default(),
                ));
            }
        }
        progress(&format!("{}/{} {} {}", index + 1, chosen.len(), merge, row.class.label()));
        rows.push(row);
    }
    let replay_seconds = replaying.elapsed().as_secs_f64();

    let mut classes = BTreeMap::new();
    for row in &rows {
        *classes.entry(row.class.label().to_string()).or_insert(0) += 1;
    }
    let verdict = judge(&rows, &tickets);
    let mut described = BTreeMap::new();
    described.insert("forklift".to_string(), crate::run::described(&forklift));
    described.insert("git".to_string(), crate::run::described(&git));
    let header = Header {
        source: source_text, reference: options.reference.clone(), commit, sample: options.sample, seed: options.seed,
        merges: Merges { replayed: rows.len(), ..merges }, rewrite, tools: described, platform: Platform::current(), os: crate::run::operating_system(),
        import_seconds, replay_seconds,
    };
    let report = ReplayReport { format: REPLAY_FORMAT, header, classes, rows, verdict };
    for (name, text) in [("replay.md", report.markdown()), ("replay.json", report.json())] {
        std::fs::write(output.root().join(name), text).map_err(|error| format!("cannot write {}: {}", name, error))?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Set by the test that shows a run stops on a warehouse it cannot shift: the settle
        /// step is skipped, leaving the warehouse as the refused abort did.
        pub(super) static SKIP_SETTLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    use crate::scrub::path_git;

    fn clean(value: &str) -> PathState {
        PathState { conflicted: false, value: value.to_string() }
    }

    fn conflicted(content: &str) -> PathState {
        PathState { conflicted: true, value: conflicted_value(content.as_bytes()) }
    }

    const FORKLIFT_MARKERS: &str = "a\n<<<<<<< main\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> side\nz\n";
    const GIT_MARKERS: &str = "a\n<<<<<<< 0123abc\nours\n||||||| 4567def\nbase\n=======\ntheirs\n>>>>>>> 89abcde\nz\n";

    /// Every path kind from constructed (Forklift, G, G1, G0) states. Divergence is decided
    /// before the gaps; conflicted files compare by content, with the marker labels stripped.
    ///
    /// Reverts: decide the gaps before comparing with G0 → the first case reads as a rename gap;
    /// skip the content comparison of conflicted-in-both paths → the conflict-content case reads
    /// as no difference; stop stripping labels → the labels-only case reads as conflict-content.
    #[test]
    fn each_path_comes_to_its_kind() {
        use Difference::*;
        let cases: [(&str, [PathState; 4], Option<Difference>); 10] = [
            ("G and G1 differ while Forklift differs from G0", [clean("x"), clean("z"), clean("y"), clean("y")], Some(Divergence { subtype: Kind::SilentWrong })),
            ("conflicted in both, differing only in labels", [conflicted(FORKLIFT_MARKERS), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS)], None),
            ("conflicted in both, a line outside the markers differs", [conflicted(FORKLIFT_MARKERS), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS), conflicted(&GIT_MARKERS.replace("\nz\n", "\nzz\n"))], Some(Divergence { subtype: Kind::ConflictContent })),
            ("clean in Forklift, conflicted in G0", [clean("x"), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS)], Some(Divergence { subtype: Kind::ForkliftCleanGitConflicted })),
            ("conflicted in Forklift, clean in G0", [conflicted(FORKLIFT_MARKERS), clean("x"), clean("x"), clean("x")], Some(Divergence { subtype: Kind::ForkliftExtraConflict })),
            ("Forklift equals G0, G and G1 differ, Forklift conflicted", [conflicted(FORKLIFT_MARKERS), clean("x"), conflicted(GIT_MARKERS), conflicted(GIT_MARKERS)], Some(RenameGap { direction: Direction::ForkliftConflicted })),
            ("Forklift equals G0, G and G1 differ, Forklift clean", [clean(PathState::ABSENT), clean("100644 r"), clean(PathState::ABSENT), clean(PathState::ABSENT)], Some(RenameGap { direction: Direction::ForkliftClean })),
            ("Forklift equals G0, G equals G1, G1 and G0 differ", [conflicted(FORKLIFT_MARKERS), clean("x"), clean("x"), conflicted(GIT_MARKERS)], Some(MultiBaseGap { direction: Direction::ForkliftConflicted })),
            ("Forklift equals G0 and G", [clean("x"), clean("x"), clean("y"), clean("x")], None),
            ("absent in Forklift, present in G0", [clean(PathState::ABSENT), clean("x"), clean("x"), clean("x")], Some(Divergence { subtype: Kind::SilentWrong })),
        ];
        for (name, [forklift, g, g1, g0], expected) in cases {
            assert_eq!(difference(&forklift, &g, &g1, &g0), expected, "{}", name);
        }
    }

    /// Only the label after each of the three markers goes; a marker-like line with no space
    /// after the marker, and the separator, stay as they are.
    #[test]
    fn only_marker_labels_are_stripped() {
        let stripped = strip_labels(b"<<<<<<< ours\n|||||||\n=======  kept\n>>>>>>> theirs\n<<<<<<<x\nend");
        assert_eq!(String::from_utf8(stripped).unwrap(), "<<<<<<<\n|||||||\n=======  kept\n>>>>>>>\n<<<<<<<x\nend");
    }

    fn path(path: &str, blob: &str, difference: Difference) -> PathRow {
        PathRow { path: path.to_string(), blob: blob.to_string(), difference, equals_committed: None }
    }

    /// Each row class from its differing paths and what both tools came to.
    #[test]
    fn each_row_comes_to_its_class() {
        let gap = path("r", "absent", Difference::RenameGap { direction: Direction::ForkliftClean });
        let divergence = path("d", "absent", Difference::Divergence { subtype: Kind::SilentWrong });
        assert_eq!(class_of(false, true, &[]), Class::Agree);
        assert_eq!(class_of(false, false, &[]), Class::HandEdited);
        assert_eq!(class_of(true, false, &[]), Class::AgreeConflicted);
        assert_eq!(class_of(true, false, std::slice::from_ref(&gap)), Class::Gap);
        assert_eq!(class_of(false, true, &[gap, divergence]), Class::Divergent);
    }

    /// G0 on a row of several bases is the single-base merge with the fewest paths differing from
    /// Forklift's, the earliest listed base on a tie.
    ///
    /// Revert: always take the first base → the first case picks 0.
    #[test]
    fn the_closest_single_base_is_chosen() {
        assert_eq!(fewest_differing(&[3, 1, 1]), Some(1));
        assert_eq!(fewest_differing(&[2, 2]), Some(0));
        assert_eq!(fewest_differing(&[]), None);
    }

    /// A merged or fast-forwarded consolidate followed by a stocktake reporting any change is a
    /// record mismatch; a clean stocktake is not, and neither is any outcome that records no tree.
    ///
    /// Revert: skip the stocktake's counts → the first case reads as no mismatch.
    #[test]
    fn a_tree_other_than_the_recorded_one_is_a_record_mismatch() {
        let stocktake = |staged: u64, unstaged: u64| serde_json::json!({"data": {"staged_count": staged, "unstaged_count": unstaged}});
        assert!(record_mismatch("merged", &stocktake(0, 1)));
        assert!(record_mismatch("fast_forward", &stocktake(2, 0)));
        assert!(record_mismatch("merged", &serde_json::json!({"data": {}})), "a stocktake without its counts is not read as clean");
        assert!(!record_mismatch("merged", &stocktake(0, 0)));
        assert!(!record_mismatch("conflicts", &stocktake(3, 3)));
        assert!(!record_mismatch("up_to_date", &stocktake(3, 3)));
    }

    const MERGE: &str = "1111111111111111111111111111111111111111";
    const BLOB: &str = "2222222222222222222222222222222222222222";

    fn row(class: Class, paths: Vec<PathRow>) -> Row {
        Row { class, paths, ..Row::new(MERGE) }
    }

    fn entry(path: &str, kind: Kind, blob: &str) -> Ticket {
        Ticket::Path(PathTicket { merge: MERGE.to_string(), path: path.to_string(), kind, blob: blob.to_string(), ticket: "FORK-539".to_string() })
    }

    fn cause(cause: Cause) -> Ticket {
        Ticket::Cause(CauseTicket { cause, ticket: "FORK-537".to_string() })
    }

    /// A divergent path fails without a path entry naming its merge, path, kind and blob, and
    /// passes with one, also when Forklift's file is the committed one; a cause entry never
    /// covers it.
    ///
    /// Revert: honour a cause entry for a divergent path → the case with only cause entries passes.
    #[test]
    fn a_divergence_needs_its_own_entry() {
        let mut divergent = path("f", BLOB, Difference::Divergence { subtype: Kind::ForkliftCleanGitConflicted });
        divergent.equals_committed = Some(true);
        let rows = [row(Class::Divergent, vec![divergent])];
        let verdict = judge(&rows, &[cause(Cause::Rename), cause(Cause::MultiBase)]);
        assert_eq!(verdict.failures.len(), 1, "{:?}", verdict);
        assert_eq!((verdict.failures[0].path.as_str(), verdict.failures[0].kind.as_str(), verdict.failures[0].blob.as_str()), ("f", "forklift-clean-git-conflicted", BLOB));
        let verdict = judge(&rows, &[entry("f", Kind::ForkliftCleanGitConflicted, BLOB)]);
        assert_eq!(verdict, Verdict { failures: Vec::new(), closable: Vec::new() });
    }

    /// An entry keyed to another blob of Forklift's at the path does not cover it, and is listed
    /// as possibly closable; so is one of another kind.
    ///
    /// Revert: match path entries without the blob → the first case passes.
    #[test]
    fn a_divergence_entry_names_forklifts_blob() {
        let rows = [row(Class::Divergent, vec![path("f", BLOB, Difference::Divergence { subtype: Kind::ConflictContent })])];
        let other_blob = entry("f", Kind::ConflictContent, "3333333333333333333333333333333333333333");
        let verdict = judge(&rows, std::slice::from_ref(&other_blob));
        assert_eq!(verdict.failures.len(), 1, "{:?}", verdict);
        assert_eq!(verdict.closable, vec![other_blob]);
        let verdict = judge(&rows, &[entry("f", Kind::SilentWrong, BLOB)]);
        assert_eq!(verdict.failures.len(), 1, "{:?}", verdict);
    }

    /// A gap fails without a cause entry for its cause and passes with one; an agree-conflicted
    /// row passes with no entry at all; a stale entry is only listed.
    #[test]
    fn gaps_take_their_cause_entry_and_stale_entries_never_fail() {
        let rows = [
            row(Class::Gap, vec![path("r", "absent", Difference::RenameGap { direction: Direction::ForkliftClean })]),
            row(Class::Gap, vec![path("m", BLOB, Difference::MultiBaseGap { direction: Direction::ForkliftConflicted })]),
        ];
        let verdict = judge(&rows, &[cause(Cause::Rename)]);
        assert_eq!(verdict.failures.iter().map(|failure| failure.kind.as_str()).collect::<Vec<_>>(), ["multi-base-gap"]);
        let verdict = judge(&rows, &[cause(Cause::Rename), cause(Cause::MultiBase)]);
        assert!(verdict.failures.is_empty(), "{:?}", verdict);

        let stale = entry("gone", Kind::SilentWrong, BLOB);
        let verdict = judge(&[row(Class::AgreeConflicted, Vec::new()), row(Class::Agree, Vec::new())], std::slice::from_ref(&stale));
        assert_eq!(verdict, Verdict { failures: Vec::new(), closable: vec![stale] });
    }

    fn row_entry(kind: Kind, reason: &str) -> Ticket {
        Ticket::Row(RowTicket { merge: MERGE.to_string(), kind, reason: reason.to_string(), ticket: "FORK-540".to_string() })
    }

    /// A forklift-refused or error row fails without a row entry naming its merge, its kind and
    /// text its reason contains, and passes with one; a both-refused row needs none; a row entry
    /// covers none of the row's paths, which are judged as on any other row.
    ///
    /// Revert: match row entries on merge and kind alone → the entry naming another reason covers
    /// the row.
    #[test]
    fn refused_and_error_rows_need_a_row_entry() {
        for (class, kind, other) in [(Class::ForkliftRefused, Kind::ForkliftRefused, Kind::Error), (Class::Error, Kind::Error, Kind::ForkliftRefused)] {
            let rows = [Row { reason: Some("aborting failed: a conflict marker".to_string()), ..row(class, Vec::new()) }];
            let verdict = judge(&rows, &[row_entry(other, "aborting failed")]);
            assert_eq!(verdict.failures.len(), 1, "{:?}", class);
            assert_eq!(verdict.failures[0].kind, kind.label());
            let verdict = judge(&rows, &[row_entry(kind, "shift failed")]);
            assert_eq!(verdict.failures.len(), 1, "another reason's entry covered the row: {:?}", class);
            assert!(judge(&rows, &[row_entry(kind, "aborting failed")]).failures.is_empty(), "{:?}", class);
        }
        assert!(judge(&[row(Class::BothRefused, Vec::new())], &[]).failures.is_empty());

        let divergent = path("f", BLOB, Difference::Divergence { subtype: Kind::SilentWrong });
        let rows = [Row { reason: Some("aborting failed".to_string()), ..row(Class::Error, vec![divergent]) }];
        let verdict = judge(&rows, &[row_entry(Kind::Error, "aborting failed")]);
        assert_eq!(verdict.failures.iter().map(|failure| (failure.path.as_str(), failure.kind.as_str())).collect::<Vec<_>>(), [("f", "silent-wrong")]);
    }

    /// The tickets this build ships with parse and pass the check, and the check refuses every
    /// entry no row could match.
    #[test]
    fn the_shipped_tickets_are_valid_and_bad_entries_are_refused() {
        let shipped = tickets().unwrap();
        assert!(shipped.iter().any(|ticket| matches!(ticket, Ticket::Cause(entry) if entry.cause == Cause::Rename)));
        assert!(shipped.iter().any(|ticket| matches!(ticket, Ticket::Cause(entry) if entry.cause == Cause::MultiBase)));
        let refused = [
            r#"[{"cause": "rename", "ticket": "FORK-1", "extra": 1}]"#,
            r#"[{"cause": "renames", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "abc", "path": "f", "kind": "silent-wrong", "blob": "absent", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "f", "kind": "error", "blob": "", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "", "kind": "error", "blob": "", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "", "kind": "silent-wrong", "blob": "absent", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "kind": "error", "reason": "", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "kind": "silent-wrong", "reason": "x", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "abc", "kind": "error", "reason": "x", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "kind": "error", "reason": "x", "ticket": "FORK-1", "path": ""}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "f", "kind": "silent-wrong", "blob": "nope", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "f", "kind": "silent-wrong", "blob": "absent", "ticket": "BUG-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "f", "kind": "record-mismatch", "blob": "", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "path": "", "kind": "record-mismatch", "blob": "absent", "ticket": "FORK-1"}]"#,
            r#"[{"merge": "1111111111111111111111111111111111111111", "kind": "record-mismatch", "reason": "x", "ticket": "FORK-1"}]"#,
        ];
        for text in refused {
            let parsed: Result<Vec<Ticket>, _> = serde_json::from_str(text);
            assert!(parsed.map_err(|error| error.to_string()).and_then(|tickets| check_tickets(&tickets)).is_err(), "{}", text);
        }
        let accepted: Vec<Ticket> = serde_json::from_str(r#"[{"merge": "1111111111111111111111111111111111111111", "kind": "forklift-refused", "reason": "refused", "ticket": "FORK-1"}]"#).unwrap();
        check_tickets(&accepted).unwrap();

        // A record-mismatch is a path row with an empty path and blob, so the entry that covers
        // it is a path entry with both empty, which the check accepts and judge matches.
        let accepted: Vec<Ticket> = serde_json::from_str(r#"[{"merge": "1111111111111111111111111111111111111111", "path": "", "kind": "record-mismatch", "blob": "", "ticket": "FORK-1"}]"#).unwrap();
        check_tickets(&accepted).unwrap();
        let mismatched = [row(Class::Divergent, vec![path("", "", Difference::Divergence { subtype: Kind::RecordMismatch })])];
        assert_eq!(judge(&mismatched, &accepted), Verdict { failures: Vec::new(), closable: Vec::new() });
        assert_eq!(judge(&mismatched, &[]).failures.len(), 1);
    }

    /// An error row whose comparison failed keeps that failure as its reason, and a clean-up
    /// failure after it apart, so a row entry for the clean-up's refusal does not cover a
    /// comparison that starts failing on the same merge; with only the clean-up failing, its
    /// failure is the reason.
    ///
    /// Revert: join the two into the reason → the refused abort's entry covers the new failure.
    #[test]
    fn a_failed_comparison_is_not_covered_by_its_clean_up_entry() {
        let refusal = "aborting the conflicted consolidate failed: has an unresolved conflict marker";
        let entry = [row_entry(Kind::Error, "has an unresolved conflict marker")];
        let mut both = row(Class::Error, Vec::new());
        both.reason = outcome(&mut both, Err("stocktake failed".to_string()), Err(refusal.to_string())).err();
        assert_eq!((both.reason.as_deref(), both.clearing.as_deref()), (Some("stocktake failed"), Some(refusal)));
        assert_eq!(judge(&[both], &entry).failures.len(), 1, "the clean-up's entry covered a failed comparison");
        let mut cleanup = row(Class::Error, Vec::new());
        cleanup.reason = outcome(&mut cleanup, Ok(()), Err(refusal.to_string())).err();
        assert_eq!(cleanup.clearing, None);
        assert!(judge(&[cleanup], &entry).failures.is_empty());
    }

    /// The probe follows exactly the rows whose consolidate was refused or which failed: the
    /// abort after them is not known to have cleared the warehouse.
    ///
    /// Revert: probe after error rows alone → a forklift-refused row is not probed.
    #[test]
    fn refused_and_error_rows_are_probed() {
        let probed: Vec<&str> = Class::ALL.into_iter().filter(|class| probed(*class)).map(Class::label).collect();
        assert_eq!(probed, ["both-refused", "forklift-refused", "error"]);
    }

    /// The sample is the first N of the merges in rev-list order, or of a splitmix64 Fisher-Yates
    /// shuffle of them under a seed. The shuffled order was computed apart from this code.
    #[test]
    fn the_sample_is_a_seeded_shuffle_then_the_first_n() {
        let merges: Vec<String> = (0..10).map(|index| index.to_string()).collect();
        assert_eq!(sampled(merges.clone(), None, None), merges);
        assert_eq!(sampled(merges.clone(), Some(3), None), ["0", "1", "2"]);
        assert_eq!(sampled(merges.clone(), None, Some(7)), ["8", "1", "5", "9", "0", "4", "3", "2", "6", "7"]);
        assert_eq!(sampled(merges, Some(2), Some(7)), ["8", "1"]);
    }

    /// The rewrite drops gitlink entries and .forkliftignore changes, at the root or nested, and
    /// keeps everything else byte for byte, a data block that looks like either included.
    #[test]
    fn the_rewrite_drops_gitlinks_and_ignore_changes_only() {
        let message = "M 160000 aaaa sub\nD .forkliftignore\n";
        let stream = format!(
            "commit refs/heads/x\nmark :1\ncommitter p <p> 0 +0000\ndata {}\n{}M 100644 :2 a\nM 160000 bbbb sub\nM 100644 :3 .forkliftignore\nD docs/.forkliftignore\nM 100644 :4 \"q/.forkliftignore\"\nM 100644 :5 x.forkliftignore\nD keep\n\n",
            message.len(), message,
        );
        let (kept, gitlinks, ignores) = rewritten(stream.as_bytes()).unwrap();
        let expected = format!("commit refs/heads/x\nmark :1\ncommitter p <p> 0 +0000\ndata {}\n{}M 100644 :2 a\nM 100644 :5 x.forkliftignore\nD keep\n\n", message.len(), message);
        assert_eq!(String::from_utf8(kept).unwrap(), expected);
        assert_eq!((gitlinks, ignores), (1, 3));
        assert!(rewritten(b"data 99\nshort").is_err());
    }

    /// A hunk is the diff's first one, cut at 40 lines with a note; the second is left out.
    #[test]
    fn a_hunk_is_the_first_cut_at_forty_lines() {
        let body: String = (0..50).map(|line| format!("+{}\n", line)).collect();
        let diff = format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1,50 @@\n{}@@ -90 +90 @@\n-x\n", body);
        let hunk = first_hunk(&diff);
        assert!(hunk.starts_with("@@ -1 +1,50 @@\n+0\n"), "{}", hunk);
        assert!(hunk.ends_with("+38\n… cut at 40 lines"), "{}", hunk);
        assert!(!hunk.contains("@@ -90"));
        assert_eq!(first_hunk("Binary files a and b differ\n"), "Binary files a and b differ");
    }

    /// A path row's kind is flattened into it in replay.json.
    #[test]
    fn a_path_row_is_written_flat() {
        let mut row = path("f", BLOB, Difference::Divergence { subtype: Kind::ForkliftCleanGitConflicted });
        row.equals_committed = Some(false);
        assert_eq!(serde_json::to_value(&row).unwrap(), serde_json::json!({
            "path": "f", "blob": BLOB, "kind": "divergence", "subtype": "forklift-clean-git-conflicted", "equals_committed": false,
        }));
        let gap = path("r", "absent", Difference::MultiBaseGap { direction: Direction::ForkliftClean });
        assert_eq!(serde_json::to_value(&gap).unwrap(), serde_json::json!({"path": "r", "blob": "absent", "kind": "multi-base-gap", "direction": "forklift-clean"}));
    }

    fn git_in(directory: &Path, arguments: &[&str], scratch: &Scratch, environment: &Environment) -> String {
        let invocation = Invocation { program: path_git(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: directory.to_path_buf(), limit: STEP_LIMIT };
        let outcome = tool::run(&invocation, scratch, environment).unwrap();
        assert!(outcome.succeeded(), "{}", said(&outcome));
        String::from_utf8_lossy(&outcome.stdout).trim().to_string()
    }

    /// Forklift's tree is hashed from its files' content every time, never from an earlier hash's
    /// stat cache. The fixture makes the cache lie on purpose: git is set to compare only size and
    /// mtime, and a file is rewritten at the same size with its mtime put back, as files of two
    /// rows can be.
    ///
    /// Revert: reuse one index across hashes → the second hash is still the first content's tree.
    #[test]
    fn the_tree_is_hashed_from_content_not_a_stat_cache() {
        let (scratch, mut environment) = crate::history::tests::scratch("replay-fresh-index");
        for (index, (key, value)) in [("core.checkStat", "minimal"), ("core.trustctime", "false")].into_iter().enumerate() {
            environment.variables.insert(format!("GIT_CONFIG_KEY_{}", index), key.to_string());
            environment.variables.insert(format!("GIT_CONFIG_VALUE_{}", index), value.to_string());
        }
        environment.variables.insert("GIT_CONFIG_COUNT".to_string(), "2".to_string());
        let mirror = scratch.root().join("mirror.git");
        git_in(scratch.root(), &["init", "-q", "--bare", &mirror.display().to_string()], &scratch, &environment);
        let warehouse = scratch.work().join("warehouse");
        std::fs::create_dir_all(&warehouse).unwrap();
        let tools = Tools { forklift: path_git(), git: path_git(), mirror, warehouse: warehouse.clone(), index: scratch.tmp().join("replay.index"), scratch: &scratch, environment: &environment };
        let mtime = std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let write = |content: &str| {
            std::fs::write(warehouse.join("a"), content).unwrap();
            std::fs::OpenOptions::new().write(true).open(warehouse.join("a")).unwrap().set_modified(mtime).unwrap();
        };
        write("one\n");
        assert_eq!(tools.forklift_tree().unwrap(), "b1ea253509c6004817f83cd7659ab4e6c38b2981");
        write("two\n");
        assert_eq!(tools.forklift_tree().unwrap(), "8f26825aadcd1e8217d7b7a489f13689fa1010d4", "the second hash read the first one's stat cache");
    }

    /// Forklift's tree is hashed from its files' bytes as they are: a .gitattributes in the tree
    /// that marks a CRLF file as text, and core.autocrlf, both of which git's add would apply,
    /// leave the file's blob as its bytes. The expected tree is built with hash-object
    /// --no-filters and mktree, apart from the code under test.
    ///
    /// Revert: drop --attr-source → the CRLF file is hashed as LF and the tree differs.
    #[test]
    fn the_tree_is_hashed_without_attributes_or_line_ending_conversion() {
        let (scratch, mut environment) = crate::history::tests::scratch("replay-no-filters");
        environment.variables.insert("GIT_CONFIG_COUNT".to_string(), "1".to_string());
        environment.variables.insert("GIT_CONFIG_KEY_0".to_string(), "core.autocrlf".to_string());
        environment.variables.insert("GIT_CONFIG_VALUE_0".to_string(), "input".to_string());
        let mirror = scratch.root().join("mirror.git");
        git_in(scratch.root(), &["init", "-q", "--bare", &mirror.display().to_string()], &scratch, &environment);
        let warehouse = scratch.work().join("warehouse");
        std::fs::create_dir_all(&warehouse).unwrap();
        std::fs::write(warehouse.join(".gitattributes"), "* text eol=lf\n").unwrap();
        std::fs::write(warehouse.join("a"), "one\r\ntwo\r\n").unwrap();
        let tools = Tools { forklift: path_git(), git: path_git(), mirror: mirror.clone(), warehouse: warehouse.clone(), index: scratch.tmp().join("replay.index"), scratch: &scratch, environment: &environment };
        let mirror_text = mirror.display().to_string();
        let blob = |name: &str| git_in(scratch.root(), &["--git-dir", &mirror_text, "hash-object", "-w", "--no-filters", &warehouse.join(name).display().to_string()], &scratch, &environment);
        let listing = format!("100644 blob {}\t.gitattributes\n100644 blob {}\ta\n", blob(".gitattributes"), blob("a"));
        let listing_path = scratch.tmp().join("listing");
        std::fs::write(&listing_path, listing).unwrap();
        let expected = String::from_utf8(tools.run(&path_git(), &["--git-dir", &mirror_text, "mktree"], scratch.root(), &environment, STEP_LIMIT, Some(&listing_path)).unwrap().stdout).unwrap();
        assert_eq!(tools.forklift_tree().unwrap(), expected.trim(), "the CRLF file was hashed through the attributes or autocrlf");
    }

    /// git's references merge without attributes, as Forklift does: a mirror whose HEAD has a
    /// `.gitattributes` setting the conflicted file's marker size to 32 still gets seven-character
    /// markers from G.
    ///
    /// Revert: drop --attr-source from the mirror's git → G's markers are 32 characters.
    #[test]
    fn git_merges_without_attributes() {
        let (scratch, environment) = crate::history::tests::scratch("replay-merge-attributes");
        let source = scratch.root().join("source");
        std::fs::create_dir(&source).unwrap();
        let git = |arguments: &[&str]| git_in(&source, arguments, &scratch, &environment);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(source.join("f"), "base\n").unwrap();
        commit(&source, "base", &scratch, &environment);
        git(&["switch", "-q", "-c", "o"]);
        std::fs::write(source.join("f"), "ours\n").unwrap();
        commit(&source, "ours", &scratch, &environment);
        git(&["switch", "-q", "-c", "t", "main"]);
        std::fs::write(source.join("f"), "theirs\n").unwrap();
        commit(&source, "theirs", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        std::fs::write(source.join(".gitattributes"), "f conflict-marker-size=32\n").unwrap();
        commit(&source, "attributes", &scratch, &environment);
        let mirror = scratch.root().join("mirror.git");
        git(&["clone", "-q", "--mirror", &source.display().to_string(), &mirror.display().to_string()]);
        let tools = Tools { forklift: path_git(), git: path_git(), mirror: mirror.clone(), warehouse: scratch.work().to_path_buf(), index: scratch.tmp().join("replay.index"), scratch: &scratch, environment: &environment };
        let merged = tools.merge_tree("o", "t", &[]).unwrap().unwrap();
        assert_eq!(merged.conflicted, BTreeSet::from(["f".to_string()]));
        let text = git_in(scratch.root(), &["--git-dir", &mirror.display().to_string(), "cat-file", "-p", &format!("{}:f", merged.tree)], &scratch, &environment);
        assert!(text.starts_with("<<<<<<< o\n"), "G's markers were sized by HEAD's attributes: {}", text);
    }

    /// A consolidate's report comes to a clean merge, its conflicted paths, or a refusal; an
    /// outcome the harness does not know, a conflict list that is missing or not a list, and a
    /// conflict that is not a path, are errors, never a refusal or a dropped path.
    ///
    /// Revert: read an unknown outcome as a refusal → the "rebased" case is Ok(Err).
    #[test]
    fn a_consolidate_report_it_cannot_read_is_an_error() {
        let report = |outcome: serde_json::Value, conflicts: serde_json::Value| serde_json::json!({"data": {"outcome": outcome, "conflicts": conflicts}});
        let null = serde_json::Value::Null;
        assert_eq!(consolidate_said(true, &report("merged".into(), null.clone())), Ok(Ok(None)));
        assert_eq!(consolidate_said(true, &report("up_to_date".into(), null.clone())), Ok(Ok(None)));
        assert_eq!(consolidate_said(true, &report("conflicts".into(), serde_json::json!(["a", "b/c"]))), Ok(Ok(Some(BTreeSet::from(["a".to_string(), "b/c".to_string()])))));
        assert_eq!(consolidate_said(false, &report("merged".into(), null.clone())), Ok(Err(())));
        for (outcome, conflicts) in [("rebased".into(), null.clone()), (null.clone(), null.clone()), ("conflicts".into(), serde_json::json!(["a", 1])), ("conflicts".into(), serde_json::json!("a")), ("conflicts".into(), null.clone())] {
            let said = consolidate_said(true, &report(outcome, conflicts.clone()));
            assert!(said.is_err(), "{:?} {:?}", conflicts, said);
        }
    }

    /// The settle step puts each conflicted file back as the first parent has it, its mode and
    /// any missing parent folder included, removes one the parent does not have, then aborts.
    /// `true` stands in for Forklift, so the abort succeeds.
    ///
    /// Revert: write every file as a plain one → the executable is not executable.
    #[cfg(unix)]
    #[test]
    fn settling_puts_files_back_with_their_modes() {
        use std::os::unix::fs::PermissionsExt;
        let (scratch, environment) = crate::history::tests::scratch("replay-settle");
        let source = scratch.root().join("source");
        std::fs::create_dir_all(source.join("x")).unwrap();
        let git = |arguments: &[&str]| git_in(&source, arguments, &scratch, &environment);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(source.join("x/run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(source.join("x/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("x/run.sh", source.join("l")).unwrap();
        commit(&source, "base", &scratch, &environment);
        let ours = git(&["rev-parse", "HEAD"]);
        let warehouse = scratch.work().join("warehouse");
        std::fs::create_dir_all(&warehouse).unwrap();
        std::fs::write(warehouse.join("gone"), "conflicted\n").unwrap();
        std::fs::write(warehouse.join("l"), "conflicted\n").unwrap();
        let tools = Tools { forklift: PathBuf::from("/usr/bin/true"), git: path_git(), mirror: source.join(".git"), warehouse: warehouse.clone(), index: scratch.tmp().join("replay.index"), scratch: &scratch, environment: &environment };
        let conflicted = BTreeSet::from(["x/run.sh".to_string(), "l".to_string(), "gone".to_string()]);
        tools.settle(&ours, &conflicted).unwrap();
        assert_eq!(std::fs::read(warehouse.join("x/run.sh")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(std::fs::metadata(warehouse.join("x/run.sh")).unwrap().permissions().mode() & 0o111, 0o111, "the executable was put back without its mode");
        assert_eq!(std::fs::read_link(warehouse.join("l")).unwrap(), PathBuf::from("x/run.sh"));
        assert!(!warehouse.join("gone").exists());
    }

    fn commit(source: &Path, message: &str, scratch: &Scratch, environment: &Environment) {
        git_in(source, &["add", "-A"], scratch, environment);
        git_in(source, &["commit", "-q", "-m", message], scratch, environment);
    }

    /// A replay of a constructed history comes to the classes its merges were built for, each
    /// counterfactual named by the git merge that shows it: a merge where one side renames a file
    /// the other edits is a rename gap; a criss-cross merge whose virtual base merges clean and
    /// whose single bases conflict is a multi-base gap; a merge of two edits to different lines,
    /// and the criss-cross's own two merges, agree; a merge conflicted on `.gitignore`, replayed
    /// first, is an error row whose consolidate the harness still gets out of.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn a_constructed_history_comes_to_its_classes() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (scratch, environment) = crate::history::tests::scratch("replay-constructed");
        let source = scratch.root().join("source");
        std::fs::create_dir(&source).unwrap();
        let git = |arguments: &[&str]| git_in(&source, arguments, &scratch, &environment);
        let write = |name: &str, content: &str| std::fs::write(source.join(name), content).unwrap();
        git(&["init", "-q", "-b", "main"]);
        write("r", "r1\nr2\nr3\nr4\nr5\n");
        write("f", "x\n");
        write("g", "x\n");
        write("p", "p1\np2\np3\np4\np5\np6\np7\n");
        commit(&source, "base", &scratch, &environment);

        git(&["switch", "-q", "-c", "renamed"]);
        git(&["mv", "r", "r2"]);
        commit(&source, "rename r", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        write("r", "r1 edited\nr2\nr3\nr4\nr5\n");
        commit(&source, "edit r", &scratch, &environment);
        git(&["merge", "-q", "--no-ff", "--no-edit", "renamed"]);
        let rename = git(&["rev-parse", "HEAD"]);

        git(&["switch", "-q", "-c", "a"]);
        write("f", "a\n");
        commit(&source, "a", &scratch, &environment);
        git(&["switch", "-q", "-c", "b", "main"]);
        write("g", "b\n");
        commit(&source, "b", &scratch, &environment);
        git(&["switch", "-q", "-c", "p2", "a"]);
        git(&["merge", "-q", "--no-ff", "--no-edit", "b"]);
        let p2 = git(&["rev-parse", "HEAD"]);
        git(&["switch", "-q", "-c", "q2", "b"]);
        git(&["merge", "-q", "--no-ff", "--no-edit", "a"]);
        let q2 = git(&["rev-parse", "HEAD"]);
        write("g", "q\n");
        commit(&source, "q3", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        git(&["merge", "-q", "--ff-only", "p2"]);
        write("f", "p\n");
        commit(&source, "p3", &scratch, &environment);
        git(&["merge", "-q", "--no-ff", "--no-edit", "q2"]);
        let criss_cross = git(&["rev-parse", "HEAD"]);

        git(&["switch", "-q", "-c", "lines"]);
        write("p", "p1 edited\np2\np3\np4\np5\np6\np7\n");
        commit(&source, "first line", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        write("p", "p1\np2\np3\np4\np5\np6\np7 edited\n");
        commit(&source, "last line", &scratch, &environment);
        git(&["merge", "-q", "--no-ff", "--no-edit", "lines"]);
        let plain = git(&["rev-parse", "HEAD"]);

        // HEAD's merge, so it is replayed first and the five above come after it.
        write(".gitignore", "a\n");
        commit(&source, "ignore a", &scratch, &environment);
        git(&["switch", "-q", "-c", "ignore"]);
        write(".gitignore", "b\n");
        commit(&source, "ignore b", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        write(".gitignore", "c\n");
        commit(&source, "ignore c", &scratch, &environment);
        git(&["merge", "-q", "-s", "ours", "--no-edit", "ignore"]);
        let ignored = git(&["rev-parse", "HEAD"]);

        let out = scratch.root().join("out");
        let options = ReplayOptions { source, reference: "HEAD".to_string(), out: out.clone(), forklift, git: path_git(), sample: None, seed: None };
        let report = run(&options, &|_| {}).unwrap();
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("replay.json")).unwrap()).unwrap();
        let rows: BTreeMap<String, &serde_json::Value> = written["rows"].as_array().unwrap().iter().map(|row| (row["merge"].as_str().unwrap().to_string(), row)).collect();
        assert_eq!(rows.len(), 6, "{}", report.markdown());
        assert_eq!(written["rows"][0]["merge"], ignored.as_str(), "{}", report.markdown());
        let paths = |merge: &str| rows[merge]["paths"].as_array().unwrap().iter()
            .map(|path| format!("{} {} {}", path["path"].as_str().unwrap(), path["kind"].as_str().unwrap(), path["direction"].as_str().unwrap_or("-")))
            .collect::<Vec<_>>();

        assert_eq!(rows[&rename]["class"], "gap", "{}", report.markdown());
        assert_eq!(paths(&rename), ["r rename-gap forklift-conflicted", "r2 rename-gap forklift-clean"]);

        assert_eq!(rows[&criss_cross]["class"], "gap", "{}", report.markdown());
        assert_eq!(rows[&criss_cross]["bases"], 2);
        assert!(rows[&criss_cross]["single_base"].is_string());
        let multi_base = paths(&criss_cross);
        assert!(multi_base == ["f multi-base-gap forklift-conflicted"] || multi_base == ["g multi-base-gap forklift-conflicted"], "{:?}", multi_base);

        for merge in [&plain, &p2, &q2] {
            assert_eq!(rows[merge]["class"], "agree", "{}: {}", merge, report.markdown());
        }

        // Forklift's abort refuses over the conflicted .gitignore: an error row, and
        // the one failure, since no entry names this merge. The rows above, all replayed after
        // it, are not errors, so the harness got the warehouse out of that consolidate.
        assert_eq!(rows[&ignored]["class"], "error", "{}", report.markdown());
        let reason = rows[&ignored]["reason"].as_str().unwrap();
        assert!(reason.starts_with("aborting the conflicted consolidate failed:") && reason.contains("unresolved conflict marker"), "{}", reason);
        assert!(!reason.contains("did not let it abort"), "{}", reason);
        // The comparison ran before the abort was refused, so the row keeps what it found.
        assert_eq!(rows[&ignored]["forklift_conflicts"], serde_json::json!([".gitignore"]), "{}", report.markdown());
        let failed: Vec<(&str, &str)> = written["verdict"]["failures"].as_array().unwrap().iter()
            .map(|failure| (failure["merge"].as_str().unwrap(), failure["kind"].as_str().unwrap())).collect();
        assert_eq!(failed, [(ignored.as_str(), "error")], "{}", report.markdown());
        assert!(report.failed());
    }

    /// A run stops, naming the merge, when an error row leaves a warehouse that cannot shift,
    /// rather than going on to make every later row an error for the same reason. The fixture's
    /// merge conflicts on .gitignore, Forklift's abort refuses over it, and the settle step is
    /// skipped, so the consolidate stays in progress.
    ///
    /// Revert: skip the probe after an error row → the run returns its report.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn a_run_stops_on_a_warehouse_it_cannot_shift() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (scratch, environment) = crate::history::tests::scratch("replay-wedged");
        let source = scratch.root().join("source");
        std::fs::create_dir(&source).unwrap();
        let git = |arguments: &[&str]| git_in(&source, arguments, &scratch, &environment);
        let write = |name: &str, content: &str| std::fs::write(source.join(name), content).unwrap();
        git(&["init", "-q", "-b", "main"]);
        write(".gitignore", "a\n");
        commit(&source, "ignore a", &scratch, &environment);
        git(&["switch", "-q", "-c", "ignore"]);
        write(".gitignore", "b\n");
        commit(&source, "ignore b", &scratch, &environment);
        git(&["switch", "-q", "main"]);
        write(".gitignore", "c\n");
        commit(&source, "ignore c", &scratch, &environment);
        git(&["merge", "-q", "-s", "ours", "--no-edit", "ignore"]);
        let ignored = git(&["rev-parse", "HEAD"]);

        let out = scratch.root().join("out");
        let options = ReplayOptions { source, reference: "HEAD".to_string(), out: out.clone(), forklift, git: path_git(), sample: None, seed: None };
        SKIP_SETTLE.with(|skip| skip.set(true));
        let stopped = run(&options, &|_| {});
        SKIP_SETTLE.with(|skip| skip.set(false));
        let reason = stopped.expect_err("the run went on past a warehouse it cannot shift");
        assert!(reason.starts_with(&format!("the warehouse cannot shift after merge {}'s error row", ignored)), "{}", reason);
        assert!(reason.contains("settling skipped by the test"), "{}", reason);
        assert!(!out.join("replay.json").exists());
    }
}
