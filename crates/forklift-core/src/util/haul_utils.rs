//! Hauls: reviewable merge proposals (pull requests), on the meta-tree substrate (`meta_tree`).
//!
//! A *haul* proposes merging a source pallet into a target pallet, with discussion and signed
//! reviews. Each haul is a directory of cells on the `@haul` meta pallet, at `<h[0..2]>/<h>/` for
//! haul id `h`:
//!
//! - `opened` — the proposal: source, target, title, description, the opener's operator id and
//!   when it was opened. The haul id is derived from it, so two hauls opened against the same
//!   target never share an id. Never changed or removed.
//! - `state` — open, closed or merged, at the proposed source head (and, once merged, the parcel
//!   the merge stacked). One cell, so a head move racing a merge or a close is a conflict on it
//!   rather than two clean changes. Only the opener moves the head; never removed, and never
//!   changed once merged.
//! - `review/<op>` — one review per operator `op` (the blake3 of its identifier), naming the head
//!   it reviewed, so a review never silently carries over to a later head.
//! - `comment/<op>/<id>` — a comment, under its writer's directory, at an id derived from it.
//!
//! A haul directory holds no cell unless its `opened` cell is present. Who wrote a cell is its
//! signer stamp; the pallet rule lets a parcel write or remove `review/` and `comment/` cells only
//! under its own signer's `<op>`, and lets only the opener write `opened`.
//!
//! Scope: intra-warehouse (pallet → pallet); reviews are recorded but merging is not gated on
//! them.

use std::collections::BTreeMap;
use chrono::Utc;
use toml_edit::{value, Table};
use crate::model::operator::Operator;
use crate::util::manifest_utils::operator_leaf;
use crate::util::meta_tree::{
    self, Admission, CellChange, CellEdit, CellEnvelope, CellRead, MetaKind, MetaSigner, MetaState, PalletRules,
    ParcelChange,
};
use crate::util::audit_utils::VerifiedOffice;
use crate::util::office_utils::{self, OfficeState};
use crate::util::{file_utils, merge_utils, object_utils, pallet_utils};

/// The name of the haul meta pallet — reached as `@haul` (DESIGN.html §3.3).
pub const HAUL_PALLET_NAME: &str = "haul";

const CELL_OPENED: &str = "opened";
const CELL_STATE: &str = "state";
const DIR_REVIEW: &str = "review";
const DIR_COMMENT: &str = "comment";

pub(crate) const SCHEMA_OPENED: &str = "haul.opened 1";
pub(crate) const SCHEMA_STATE: &str = "haul.state 1";
pub(crate) const SCHEMA_REVIEW: &str = "haul.review 1";
pub(crate) const SCHEMA_COMMENT: &str = "haul.comment 1";

const FIELD_SOURCE: &str = "source";
const FIELD_TARGET: &str = "target";
const FIELD_TITLE: &str = "title";
const FIELD_DESCRIPTION: &str = "description";
const FIELD_OPENER: &str = "opener";
const FIELD_OPENED_AT: &str = "opened_at";
const FIELD_HEAD: &str = "head";
const FIELD_STATE: &str = "state";
const FIELD_MERGE_PARCEL: &str = "merge_parcel";
const FIELD_VERDICT: &str = "verdict";
const FIELD_BODY: &str = "body";
const FIELD_REVIEWED_AT: &str = "reviewed_at";
const FIELD_COMMENTED_AT: &str = "commented_at";

/// A review's verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "approve",
            ReviewVerdict::RequestChanges => "request-changes",
            ReviewVerdict::Comment => "comment",
        }
    }

    pub fn parse(value: &str) -> Result<ReviewVerdict, String> {
        match value {
            "approve" => Ok(ReviewVerdict::Approve),
            "request-changes" => Ok(ReviewVerdict::RequestChanges),
            "comment" => Ok(ReviewVerdict::Comment),
            other => Err(format!("\"{}\" is not a review verdict (approve | request-changes | comment).", other)),
        }
    }
}

/// The status of a haul.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HaulStatus {
    Open,
    Merged(String),
    Closed,
}

impl HaulStatus {
    /// The status's name, as `haul list --state` spells it.
    pub fn as_str(&self) -> &'static str {
        match self {
            HaulStatus::Open => "open",
            HaulStatus::Merged(_) => "merged",
            HaulStatus::Closed => "closed",
        }
    }
}

/// The proposal itself — the `opened` cell's value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Opened {
    pub source: String,
    pub target: String,
    pub title: String,
    pub description: String,

    /// The operator who opened the haul. The content check holds it equal to the operator of
    /// the key that signed the cell.
    pub opener: String,

    /// When the haul was opened. Part of the haul's identity, never a security input.
    pub opened_at: i64,
}

impl Opened {
    /// The haul id: the blake3 of every field, each length-prefixed so no two field lists share
    /// an encoding.
    pub fn id(&self) -> String {
        let mut hasher = blake3::Hasher::new();

        for field in [&self.source, &self.target, &self.title, &self.description, &self.opener] {
            hasher.update(&(field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }

        hasher.update(&self.opened_at.to_le_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

/// A review, as its writer last recorded it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Review {
    /// The operator whose key the cell is stamped with, or the key id when the office does not
    /// know that key.
    pub author: String,

    pub verdict: ReviewVerdict,
    pub body: String,

    /// The haul head the review is of.
    pub head: String,

    /// When it was recorded — display metadata only.
    pub reviewed_at: i64,
}

/// A comment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Comment {
    /// As [`Review::author`].
    pub author: String,

    pub body: String,

    /// When it was recorded — display metadata only.
    pub commented_at: i64,
}

/// A haul cell this build cannot read: a newer schema or cell, or a malformed one. It is
/// listed, never dropped and never allowed to fail a read of the others.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableCell {
    /// The cell path, relative to `cells/`.
    pub path: String,

    /// Why it cannot be read.
    pub reason: String,
}

/// A haul: the state of one proposal's cells.
#[derive(Clone, Debug)]
pub struct Haul {
    pub id: String,
    pub opened: Opened,

    /// The current proposed head.
    pub head: String,

    pub status: HaulStatus,

    /// One review per writer, in path order.
    pub reviews: Vec<Review>,

    /// Every comment, oldest first.
    pub comments: Vec<Comment>,

    /// The haul's review and comment cells this build cannot read.
    pub unreadable: Vec<UnreadableCell>,
}

impl Haul {
    /// The reviews of the current head; a review of an earlier head no longer speaks for it.
    pub fn current_reviews(&self) -> impl Iterator<Item = &Review> {
        self.reviews.iter().filter(|review| review.head == self.head)
    }
}

/// A haul whose `opened` or `state` cell this build cannot read. Nothing about it is
/// shown as though it were known, and every command addressing it refuses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableHaul {
    pub id: String,

    /// Why it cannot be read, naming the cell.
    pub reason: String,
}

/// Every haul at one head.
#[derive(Clone, Debug, Default)]
pub struct HaulsRead {
    /// Every readable haul, newest first by open time.
    pub hauls: Vec<Haul>,

    /// Every haul this build cannot read.
    pub unreadable: Vec<UnreadableHaul>,

    /// Cells outside any haul's known shape (a path this build cannot place).
    pub stray: Vec<UnreadableCell>,
}

/// The directory of a haul, relative to `cells/`.
pub(crate) fn haul_dir(id: &str) -> String {
    format!("{}/{}", &id[..2], id)
}

/// The cell path of a comment by `operator`.
pub(crate) fn comment_path(id: &str, operator: &str, comment: &Table) -> String {
    format!("{}/{}/{}/{}", haul_dir(id), DIR_COMMENT, operator_leaf(operator), comment_id(comment))
}

/// The id of a comment: the blake3 of its value's fields, each length-prefixed. The writer is
/// not part of it — the id sits under the writer's own directory.
fn comment_id(comment: &Table) -> String {
    let body = meta_tree::value_string(comment, FIELD_BODY).unwrap_or_default();
    let at = meta_tree::value_integer(comment, FIELD_COMMENTED_AT).unwrap_or_default();

    let mut hasher = blake3::Hasher::new();
    hasher.update(&(body.len() as u64).to_le_bytes());
    hasher.update(body.as_bytes());
    hasher.update(&at.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

/// One cell of a haul, by where it sits.
#[derive(Clone, PartialEq, Eq, Debug)]
enum CellName<'a> {
    Opened,
    State,

    /// `review/<op>`.
    Review { owner: &'a str },

    /// `comment/<op>/<id>`.
    Comment { owner: &'a str, id: &'a str },

    /// A cell name this build does not know — written by a newer forklift.
    Other,
}

/// The parts of a haul cell path.
struct CellPath<'a> {
    haul: &'a str,
    name: CellName<'a>,
}

/// Split a cell path, refusing any path that is not under `<h[0..2]>/<h>/` for a 64-hex haul id,
/// or that puts a known cell at a shape it does not take.
fn parse_cell_path(path: &str) -> Result<CellPath<'_>, String> {
    let parts: Vec<&str> = path.split('/').collect();

    let [fan, haul, rest @ ..] = parts.as_slice() else {
        return Err(format!("{} is not a cell under a haul's directory", path));
    };

    if !object_utils::is_object_hash(haul) || fan != &&haul[..2] {
        return Err(format!("{} does not sit under the directory of a haul id", path));
    }

    let hex = |part: &str| object_utils::is_object_hash(part);

    let name = match rest {
        [CELL_OPENED] => CellName::Opened,
        [CELL_STATE] => CellName::State,
        [DIR_REVIEW, owner] if hex(owner) => CellName::Review { owner },
        [DIR_COMMENT, owner, id] if hex(owner) && hex(id) => CellName::Comment { owner, id },
        [first, ..] if ![CELL_OPENED, CELL_STATE, DIR_REVIEW, DIR_COMMENT].contains(first) =>
            CellName::Other,
        _ => return Err(format!("{} is not at a shape its cell takes", path)),
    };

    Ok(CellPath { haul, name })
}

/// The schema a known cell name holds.
fn schema_of(name: &CellName) -> Option<&'static str> {
    match name {
        CellName::Opened => Some(SCHEMA_OPENED),
        CellName::State => Some(SCHEMA_STATE),
        CellName::Review { .. } => Some(SCHEMA_REVIEW),
        CellName::Comment { .. } => Some(SCHEMA_COMMENT),
        CellName::Other => None,
    }
}

/// A full parcel hash in a value field.
fn value_hash(table: &Table, field: &str) -> Result<String, String> {
    let hash = meta_tree::value_string(table, field)?;

    if !object_utils::is_object_hash(&hash) {
        return Err(format!("has a {} field that is not a full parcel hash", field));
    }

    Ok(hash)
}

pub(crate) fn opened_value(opened: &Opened) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_SOURCE, value(opened.source.as_str()));
    table.insert(FIELD_TARGET, value(opened.target.as_str()));
    table.insert(FIELD_TITLE, value(opened.title.as_str()));
    table.insert(FIELD_DESCRIPTION, value(opened.description.as_str()));
    table.insert(FIELD_OPENER, value(opened.opener.as_str()));
    table.insert(FIELD_OPENED_AT, value(opened.opened_at));

    table
}

fn parse_opened(table: &Table) -> Result<Opened, String> {
    meta_tree::value_fields_only(table, &[
        FIELD_SOURCE, FIELD_TARGET, FIELD_TITLE, FIELD_DESCRIPTION, FIELD_OPENER, FIELD_OPENED_AT,
    ])?;

    Ok(Opened {
        source: meta_tree::value_string(table, FIELD_SOURCE)?,
        target: meta_tree::value_string(table, FIELD_TARGET)?,
        title: meta_tree::value_string(table, FIELD_TITLE)?,
        description: meta_tree::value_string(table, FIELD_DESCRIPTION)?,
        opener: meta_tree::value_string(table, FIELD_OPENER)?,
        opened_at: meta_tree::value_integer(table, FIELD_OPENED_AT)?,
    })
}

pub(crate) fn state_value(status: &HaulStatus, head: &str) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_STATE, value(status.as_str()));
    table.insert(FIELD_HEAD, value(head));

    if let HaulStatus::Merged(parcel) = status {
        table.insert(FIELD_MERGE_PARCEL, value(parcel.as_str()));
    }

    table
}

/// Parse a state strictly: the status and the head it is of, with a merge parcel exactly when
/// merged.
fn parse_state(table: &Table) -> Result<(HaulStatus, String), String> {
    let status = match meta_tree::value_string(table, FIELD_STATE)?.as_str() {
        "merged" => {
            meta_tree::value_fields_only(table, &[FIELD_STATE, FIELD_HEAD, FIELD_MERGE_PARCEL])?;
            HaulStatus::Merged(value_hash(table, FIELD_MERGE_PARCEL)?)
        }
        state @ ("open" | "closed") => {
            meta_tree::value_fields_only(table, &[FIELD_STATE, FIELD_HEAD])?;
            if state == "open" { HaulStatus::Open } else { HaulStatus::Closed }
        }
        other => return Err(format!("has the state \"{}\", which is not open, closed or merged", other)),
    };

    Ok((status, value_hash(table, FIELD_HEAD)?))
}

pub(crate) fn review_value(verdict: ReviewVerdict, body: &str, head: &str, reviewed_at: i64) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_VERDICT, value(verdict.as_str()));
    table.insert(FIELD_BODY, value(body));
    table.insert(FIELD_HEAD, value(head));
    table.insert(FIELD_REVIEWED_AT, value(reviewed_at));

    table
}

/// Parse a review, with `author` filled in by the caller.
fn parse_review(table: &Table) -> Result<Review, String> {
    meta_tree::value_fields_only(table, &[FIELD_VERDICT, FIELD_BODY, FIELD_HEAD, FIELD_REVIEWED_AT])?;

    Ok(Review {
        author: String::new(),
        verdict: ReviewVerdict::parse(&meta_tree::value_string(table, FIELD_VERDICT)?)
            .map_err(|why| format!("has a verdict field that does not parse: {}", why))?,
        body: meta_tree::value_string(table, FIELD_BODY)?,
        head: value_hash(table, FIELD_HEAD)?,
        reviewed_at: meta_tree::value_integer(table, FIELD_REVIEWED_AT)?,
    })
}

pub(crate) fn comment_value(body: &str, commented_at: i64) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_BODY, value(body));
    table.insert(FIELD_COMMENTED_AT, value(commented_at));

    table
}

/// Parse a comment, with `author` filled in by the caller.
fn parse_comment(table: &Table) -> Result<Comment, String> {
    meta_tree::value_fields_only(table, &[FIELD_BODY, FIELD_COMMENTED_AT])?;

    Ok(Comment {
        author: String::new(),
        body: meta_tree::value_string(table, FIELD_BODY)?,
        commented_at: meta_tree::value_integer(table, FIELD_COMMENTED_AT)?,
    })
}

/// Check a known cell's value strictly, by the schema its name holds.
fn check_value(name: &CellName, table: &Table) -> Result<(), String> {
    match name {
        CellName::Opened => parse_opened(table).map(|_| ()),
        CellName::State => parse_state(table).map(|_| ()),
        CellName::Review { .. } => parse_review(table).map(|_| ()),
        CellName::Comment { .. } => parse_comment(table).map(|_| ()),
        CellName::Other => Ok(()),
    }
}

/// The state of a haul in a `cells/` tree, or `None` when it has none. A state this build cannot
/// read is an error, since a rule that needs it cannot tell whether it reads merged.
fn state_in(cells: Option<&str>, dir: &str, rules: &HaulRules) -> Result<Option<(HaulStatus, String)>, String> {
    let path = format!("{}/{}", dir, CELL_STATE);

    match meta_tree::read_cell_in(cells, &path, rules)? {
        None => Ok(None),
        Some(CellRead::Readable(envelope)) if envelope.schema == SCHEMA_STATE =>
            parse_state(&envelope.value).map(Some).map_err(|why| format!("the state at {} {}", path, why)),
        Some(_) => Err(format!(
            "the state at {} cannot be read by this forklift, so it cannot tell whether the haul \
            was merged",
            path
        )),
    }
}

/// The proposal of a haul in a `cells/` tree, or `None` when it has none.
fn opened_in(cells: Option<&str>, dir: &str, rules: &HaulRules) -> Result<Option<Opened>, String> {
    let path = format!("{}/{}", dir, CELL_OPENED);

    match meta_tree::read_cell_in(cells, &path, rules)? {
        None => Ok(None),
        Some(CellRead::Readable(envelope)) if envelope.schema == SCHEMA_OPENED =>
            parse_opened(&envelope.value).map(Some).map_err(|why| format!("the proposal at {} {}", path, why)),
        Some(_) => Err(format!("the proposal at {} cannot be read by this forklift", path)),
    }
}

/// The pallet rules of `@haul`:
///
/// - `opened` is written once, by its opener, at the id its content derives, together with a
///   `state`, and is never changed or removed;
/// - a haul directory holds no other cell unless its `opened` cell is present;
/// - `state` is never removed, never changed once it reads merged, and its head is moved only by
///   the opener — so a merge records the head it merged, and a head move racing a merge is a
///   conflict on the one cell;
/// - a state that reads merged is admitted only when its signer may write the haul's target
///   ([`may_record_merge`]), judged against the office as it stands — under
///   [`Admission::Admit`] only, so adopting history a remote already admitted never wedges on a
///   merger demoted since;
/// - `review/<op>` and `comment/<op>/…` are written or removed only by the operator `op` names;
/// - `opened` and `state` hold exactly their own schema, since the rules read them; a review or
///   comment of a newer schema is carried; and no cell name this build does not know is written.
pub struct HaulRules;

impl PalletRules for HaulRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Haul
    }

    fn knows_schema(&self, schema: &str) -> bool {
        [SCHEMA_OPENED, SCHEMA_STATE, SCHEMA_REVIEW, SCHEMA_COMMENT].contains(&schema)
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        let operator = &change.signer.operator;
        let owner = operator_leaf(operator);

        // Each haul's proposal after the parcel, read once per haul.
        let mut proposals: BTreeMap<String, Option<Opened>> = BTreeMap::new();

        for cell in change.changes {
            let path = parse_cell_path(&cell.path)?;
            let dir = haul_dir(path.haul);

            match &path.name {
                CellName::Opened if cell.before.is_some() => return Err(format!(
                    "{} is the proposal of a haul, which is never changed or removed", cell.path
                )),
                CellName::State if cell.after.is_none() => return Err(format!(
                    "{} is the state of a haul, which is never removed", cell.path
                )),
                CellName::Review { owner: written } | CellName::Comment { owner: written, .. }
                    if *written != owner => return Err(format!(
                        "the cell at {} sits under another operator's directory, so only they may \
                        write or remove it",
                        cell.path
                    )),
                // A cell no rule here covers would be stored unchecked; a newer forklift's cell
                // waits for a remote that knows its rules.
                CellName::Other => return Err(format!(
                    "{} is not a cell this forklift knows a haul to hold", cell.path
                )),
                _ => {}
            }

            // The state this parcel changes, as its parent left it.
            let before = match (&path.name, &cell.before) {
                (CellName::State, Some(_)) => state_in(change.parent_cells, &dir, self)?,
                _ => None,
            };

            if let Some((HaulStatus::Merged(_), _)) = before {
                return Err(format!("the haul {} is merged, and a merge is final", path.haul));
            }

            let Some(after) = &cell.after else { continue };

            let opened = match proposals.get(&dir) {
                Some(opened) => opened.clone(),
                None => {
                    let opened = opened_in(change.cells, &dir, self)?;
                    proposals.insert(dir.clone(), opened.clone());
                    opened
                }
            };

            let Some(opened) = opened else {
                return Err(format!(
                    "the cell at {} belongs to a haul with no proposal; resolve the whole haul \
                    (its id) rather than its proposal alone",
                    cell.path
                ));
            };

            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;
            let Some(schema) = schema_of(&path.name) else { continue };

            if envelope.schema != schema {
                // A review or comment of a newer schema is carried; the cells the rules read
                // hold exactly their own.
                let carried = matches!(path.name, CellName::Review { .. } | CellName::Comment { .. });

                if self.knows_schema(&envelope.schema) || !carried {
                    return Err(format!("the cell at {} holds a {} value", cell.path, envelope.schema));
                }

                continue;
            }

            check_value(&path.name, &envelope.value).map_err(|why| format!("the value at {} {}", cell.path, why))?;

            match &path.name {
                CellName::Opened => {
                    if opened.id() != path.haul {
                        return Err(format!("the proposal at {} is not at the id its content derives", cell.path));
                    }

                    if &opened.opener != operator {
                        return Err(format!(
                            "the proposal at {} names {} as its opener, but is signed by {}",
                            cell.path, opened.opener, operator
                        ));
                    }

                    // A haul is opened open: never born closed, or merged without a merge.
                    match state_in(change.cells, &dir, self)? {
                        None => return Err(format!("the haul {} is opened without a state", path.haul)),
                        Some((HaulStatus::Open, _)) => {}
                        Some((status, _)) => return Err(format!(
                            "the haul {} is opened {}, but a haul is opened open", path.haul, status.as_str()
                        )),
                    }
                }
                CellName::State => {
                    let (status, moved) = parse_state(&envelope.value)?;

                    // A state written with its proposal is the opener's by the proposal's own rule.
                    if let Some((_, head)) = &before {
                        if &moved != head && &opened.opener != operator {
                            return Err(format!(
                                "the state at {} moves the head, but is signed by {}; only the haul's \
                                opener, {}, moves it. A merge or close that raced the opener's update \
                                keeps theirs at {} and runs again",
                                cell.path, operator, opened.opener, self.display_path(&cell.path)
                            ));
                        }
                    }

                    let refusal = match status {
                        HaulStatus::Merged(_) if change.admission == Admission::Admit => {
                            may_record_merge(change.office, operator, &opened.target).err()
                        }
                        _ => None,
                    };

                    if let Some(refusal) = refusal {
                        return Err(format!(
                            "the state at {} records a merge into {}, but is signed by {}, who {} it",
                            cell.path, opened.target, operator, refusal.why()
                        ));
                    }
                }
                CellName::Comment { id, .. } if comment_id(&envelope.value) != *id => {
                    return Err(format!("the comment at {} is not at the id its content derives", cell.path));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// `<id>` or any path beneath it, for a full haul id.
    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        let haul = spelled.split('/').next().unwrap_or_default();

        if !object_utils::is_object_hash(haul) {
            return Err(format!(
                "\"{}\" does not name a haul by its full id (64 lowercase hex characters).", haul
            ));
        }

        meta_tree::validate_cell_path(spelled)?;

        Ok(format!("{}/{}", &haul[..2], spelled))
    }

    fn display_path(&self, path: &str) -> String {
        match path.split_once('/') {
            Some((_fan, rest)) => rest.to_string(),
            None => path.to_string(),
        }
    }

    fn display_value(&self, envelope: &CellEnvelope) -> String {
        let table = &envelope.value;

        let shown = match envelope.schema.as_str() {
            SCHEMA_OPENED => parse_opened(table).map(|opened| format!("\"{}\" ({} → {})", opened.title, opened.source, opened.target)),
            SCHEMA_STATE => parse_state(table).map(|(status, head)| match status {
                HaulStatus::Merged(parcel) => format!("merged as {} at head {}", parcel, head),
                status => format!("{} at head {}", status.as_str(), head),
            }),
            SCHEMA_REVIEW => parse_review(table).map(|review| format!("{} of {}", review.verdict.as_str(), review.head)),
            SCHEMA_COMMENT => parse_comment(table).map(|comment| comment.body),
            _ => Ok(table.to_string()),
        };

        shown.unwrap_or_else(|why| format!("(unreadable: the value {})", why))
    }
}

/// Whether `operator` may record a merge into the pallet `target`: their office record exists
/// and may write it. A merge records that the target took the haul, so recording one is the
/// target's writers' to do — the same predicate that authorizes moving the target itself.
pub fn may_record_merge(office: &OfficeState, operator: &str, target: &str) -> Result<(), MergeRefusal> {
    match office.find_user(operator) {
        Some(user) if user.may_write_pallet(target) => Ok(()),
        Some(_) => Err(MergeRefusal::NotAWriter),
        None => Err(MergeRefusal::NoUserRecord),
    }
}

/// Why [`may_record_merge`] refused an operator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeRefusal {
    /// Their office record may not write the target.
    NotAWriter,
    /// The office has no user record for them.
    NoUserRecord,
}

impl MergeRefusal {
    /// The refusal as a predicate on the operator, ending before the target: "may not write".
    pub fn why(self) -> &'static str {
        match self {
            MergeRefusal::NotAWriter => "may not write",
            MergeRefusal::NoUserRecord => "has no user record in the office, so may not write",
        }
    }
}

/// What the local warehouse proves about a merged haul's recorded merge. A record is a signed
/// claim by whoever may record it; this is the evidence for it, judged on read against what this
/// warehouse holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MergeProof {
    /// The merge parcel contains the haul's head, and the target here contains the parcel.
    Verified,

    /// The merge parcel is held here and does not contain the haul's head.
    Refuted,

    /// Neither, for the reason given.
    Unproven(Unproven),
}

/// Why a merge is neither verified nor refuted here. None of these says the record is false: a
/// warehouse behind the remote reads exactly as one handed a false record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unproven {
    /// The merge parcel is not held here.
    ParcelNotHeld,

    /// The target pallet has no head here.
    TargetNotHeld,

    /// The target here does not contain the merge parcel.
    NotOnTarget,

    /// The history the answer needs could not be read.
    HistoryUnreadable,
}

impl MergeProof {
    /// The verdict, as `--json` spells it: `verified`, `refuted` or `unproven`.
    pub fn verdict(&self) -> &'static str {
        match self {
            MergeProof::Verified => "verified",
            MergeProof::Refuted => "refuted",
            MergeProof::Unproven(_) => "unproven",
        }
    }

    /// Why the verdict is not `verified`, as `--json` spells it; `None` when it is.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            MergeProof::Verified => None,
            MergeProof::Refuted => Some("head_not_contained"),
            MergeProof::Unproven(Unproven::ParcelNotHeld) => Some("parcel_not_held"),
            MergeProof::Unproven(Unproven::TargetNotHeld) => Some("target_not_held"),
            MergeProof::Unproven(Unproven::NotOnTarget) => Some("not_on_target"),
            MergeProof::Unproven(Unproven::HistoryUnreadable) => Some("history_unreadable"),
        }
    }
}

/// Judge a merged haul's record against the local warehouse; `None` for a haul not merged.
///
/// Refuted only from the merge parcel's own history, which `merge_utils::is_ancestor` answers
/// `false` for only once it was read in full. A parcel absent from the target is never refuted.
pub fn merge_proof(haul: &Haul) -> Option<MergeProof> {
    let HaulStatus::Merged(parcel) = &haul.status else { return None };

    let proof = match file_utils::does_object_exist(parcel) {
        Ok(true) => match merge_utils::is_ancestor(&haul.head, parcel) {
            Ok(false) => MergeProof::Refuted,
            Err(_) => MergeProof::Unproven(Unproven::HistoryUnreadable),
            Ok(true) => match pallet_utils::get_pallet_head(&haul.opened.target) {
                Ok(None) => MergeProof::Unproven(Unproven::TargetNotHeld),
                Err(_) => MergeProof::Unproven(Unproven::HistoryUnreadable),
                Ok(Some(target_head)) => match merge_utils::is_ancestor(parcel, &target_head) {
                    Ok(true) => MergeProof::Verified,
                    Ok(false) => MergeProof::Unproven(Unproven::NotOnTarget),
                    Err(_) => MergeProof::Unproven(Unproven::HistoryUnreadable),
                },
            },
        },
        Ok(false) => MergeProof::Unproven(Unproven::ParcelNotHeld),
        Err(_) => MergeProof::Unproven(Unproven::HistoryUnreadable),
    };

    Some(proof)
}

/// A merge record a lift of `@haul` holds back: its merge parcel is on the target here, and the
/// remote's target is not known to hold it yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeldMerge {
    pub haul: String,
    pub parcel: String,
    pub target: String,
    /// Whether the remote holds a state of the haul, which keeping theirs restores.
    pub on_remote: bool,
}

/// The merge records among `changes` — the cells an `@haul` lift would publish over the remote's
/// head — that must wait for their merge parcel, so a remote never holds a final merge whose
/// parcel was about to follow it. A record waits when its parcel is in the history of the local
/// target and the remote's target head (`remote_target_head`, by pallet name) is absent, not held
/// here, or held without it. A record whose parcel is not on the local target, or whose target is
/// not held here, goes through: waiting would not bring that parcel, and a reader judges it on
/// read ([`merge_proof`]).
///
/// `local_cells` is the local `@haul` cells tree, where each changed state's proposal is read.
pub fn held_merges(changes: &[CellChange],
                   local_cells: Option<&str>,
                   remote_target_head: &dyn Fn(&str) -> Option<String>) -> Result<Vec<HeldMerge>, String> {
    let mut held = Vec::new();

    for change in changes {
        let Some(after) = &change.after else { continue };
        let Ok(CellPath { haul, name: CellName::State }) = parse_cell_path(&change.path) else { continue };

        let CellRead::Readable(state) = meta_tree::read_cell_blob(after, &HaulRules)? else { continue };
        if state.schema != SCHEMA_STATE {
            continue;
        }
        let Ok((HaulStatus::Merged(parcel), _)) = parse_state(&state.value) else { continue };

        let Some(opened) = opened_in(local_cells, &haul_dir(haul), &HaulRules)? else { continue };

        // A target this warehouse cannot name holds nothing here, so waiting brings nothing.
        let Ok(Some(local_target)) = pallet_utils::get_pallet_head(&opened.target) else { continue };
        let unjudged = |why: String| format!(
            "could not judge whether the merge of haul {} into \"{}\" waits for its parcel: {}",
            short(haul), opened.target, why
        );

        if !merge_utils::is_ancestor(&parcel, &local_target).map_err(unjudged)? {
            continue;
        }

        let contained = match remote_target_head(&opened.target) {
            Some(remote) if file_utils::does_object_exist(&remote).map_err(unjudged)? => {
                merge_utils::is_ancestor(&parcel, &remote).map_err(unjudged)?
            }
            _ => false,
        };

        if !contained {
            let on_remote = change.before.is_some();
            held.push(HeldMerge { haul: haul.to_string(), parcel, target: opened.target, on_remote });
        }
    }

    Ok(held)
}

/// The merge records a lift of the local `@haul` head `local_head` over the remote's head
/// `remote_head` holds back ([`held_merges`]). None when the remote's head is not held here, or
/// is held but not in the local head's history: that lift is a divergence, which the lift itself
/// refuses (naming "lower") before anything is published.
pub fn held_by_lift(local_head: &str,
                    remote_head: Option<&str>,
                    remote_target_head: &dyn Fn(&str) -> Option<String>) -> Result<Vec<HeldMerge>, String> {
    let before = match remote_head {
        Some(remote) if !file_utils::does_object_exist(remote)? => return Ok(Vec::new()),
        Some(remote) if !merge_utils::is_ancestor(remote, local_head)? => return Ok(Vec::new()),
        Some(remote) => meta_tree::cells_at(HAUL_PALLET_NAME, remote, &HaulRules)?,
        None => None,
    };
    let after = meta_tree::cells_at(HAUL_PALLET_NAME, local_head, &HaulRules)?;

    let changes = meta_tree::net_diff(before.as_deref(), after.as_deref())?;
    held_merges(&changes, after.as_deref(), remote_target_head)
}

/// The fact of held merge records, for a caller to add its way out to.
pub fn describe_held(held: &[HeldMerge]) -> String {
    let records: Vec<String> = held.iter().map(|merge| format!(
        "haul {} merged as {} into \"{}\"", short(&merge.haul), short(&merge.parcel), merge.target
    )).collect();

    format!(
        "@{} was not lifted: it records a merge whose parcel the remote's target does not hold yet ({})",
        HAUL_PALLET_NAME, records.join("; ")
    )
}

/// The commands that drop each held merge record by keeping the remote's side of it: its state
/// when the remote holds one, else the haul, which the remote does not hold yet. Each takes a
/// replay without the record, so the record is no longer held; a replay with nothing else to
/// publish is empty, and needs no signature.
pub fn drop_held(held: &[HeldMerge]) -> String {
    let drops: Vec<String> = held.iter().map(|merge| match merge.on_remote {
        true => format!("\"forklift lower --keep @{}:{}/state=theirs --confirm\"", HAUL_PALLET_NAME, merge.haul),
        false => format!(
            "\"forklift lower --keep @{}:{}=theirs --confirm\" (which drops the haul, not on the remote yet)",
            HAUL_PALLET_NAME, merge.haul
        ),
    }).collect();

    drops.join(" and ")
}

/// Stack edits to the local `@haul` head as one signed parcel. `None` when they changed nothing.
fn stack(edits: &[CellEdit],
         office: &VerifiedOffice,
         actor: &Operator,
         description: &str,
         signer: &MetaSigner) -> Result<Option<String>, String> {
    meta_tree::stack_meta_edits(HAUL_PALLET_NAME, &HaulRules, office, edits, actor, description, signer)
}

/// The operator a signing key belongs to.
pub fn signing_operator(office: &OfficeState, signer: &MetaSigner) -> Result<String, String> {
    Ok(office.find_key(&signer.key_id)
        .ok_or(format!("The signing key {} is not in the office.", signer.key_id))?
        .operator
        .clone())
}

/// Open a haul: its proposal and its state — open, at `head` — as one signed parcel. The caller holds the warehouse
/// lock and passes the verified office. Returns the new haul id.
pub fn open_haul(source: &str,
                 target: &str,
                 head: &str,
                 title: &str,
                 description: &str,
                 office: &VerifiedOffice,
                 actor: &Operator,
                 signer: &MetaSigner) -> Result<String, String> {
    let opened = Opened {
        source: source.to_string(),
        target: target.to_string(),
        title: title.to_string(),
        description: description.to_string(),
        opener: signing_operator(office, signer)?,
        opened_at: Utc::now().timestamp(),
    };

    let id = opened.id();
    let dir = haul_dir(&id);

    let edits = [
        meta_tree::set_cell(&format!("{}/{}", dir, CELL_OPENED), &signer.key_id, SCHEMA_OPENED, &opened_value(&opened))?,
        meta_tree::set_cell(&format!("{}/{}", dir, CELL_STATE), &signer.key_id, SCHEMA_STATE, &state_value(&HaulStatus::Open, head))?,
    ];

    stack(&edits, office, actor, &format!("Opened haul \"{}\".", title), signer)?
        .ok_or(format!("Haul {} is already open.", short(&id)))?;

    Ok(id)
}

/// Record a comment on a haul.
pub fn record_comment(haul_id: &str,
                      body: &str,
                      office: &VerifiedOffice,
                      actor: &Operator,
                      signer: &MetaSigner) -> Result<Option<String>, String> {
    let comment = comment_value(body, Utc::now().timestamp());
    let path = comment_path(haul_id, &signing_operator(office, signer)?, &comment);
    let edit = meta_tree::set_cell(&path, &signer.key_id, SCHEMA_COMMENT, &comment)?;

    stack(&[edit], office, actor, &format!("Commented on haul {}.", short(haul_id)), signer)
}

/// Record the signer's review of a haul's `head`, replacing any earlier review of theirs.
pub fn record_review(haul_id: &str,
                     verdict: ReviewVerdict,
                     body: &str,
                     head: &str,
                     office: &VerifiedOffice,
                     actor: &Operator,
                     signer: &MetaSigner) -> Result<Option<String>, String> {
    let path = format!("{}/{}/{}", haul_dir(haul_id), DIR_REVIEW, operator_leaf(&signing_operator(office, signer)?));
    let review = review_value(verdict, body, head, Utc::now().timestamp());
    let edit = meta_tree::set_cell(&path, &signer.key_id, SCHEMA_REVIEW, &review)?;

    stack(&[edit], office, actor, &format!("Reviewed haul {} ({}).", short(haul_id), verdict.as_str()), signer)
}

/// Record a haul's state: `status` at `head`, so a move of the head and a change of status are
/// one cell and race as one. `action` names the change in the parcel's description ("Updated",
/// "Closed", …). `None` when the state is already that.
pub fn record_state(haul_id: &str,
                    status: &HaulStatus,
                    head: &str,
                    action: &str,
                    office: &VerifiedOffice,
                    actor: &Operator,
                    signer: &MetaSigner) -> Result<Option<String>, String> {
    let path = format!("{}/{}", haul_dir(haul_id), CELL_STATE);
    let edit = meta_tree::set_cell(&path, &signer.key_id, SCHEMA_STATE, &state_value(status, head))?;

    stack(&[edit], office, actor, &format!("{} haul {}.", action, short(haul_id)), signer)
}

/// The heads every haul not yet merged proposes, at the local `@haul` head: the gc roots a haul
/// adds. `haul consolidate` merges the recorded head, not the source pallet's current one, and no ref
/// need still reach it once the source moves on, so it stays live while the haul can still be
/// merged — a closed haul included, since reopening it proposes the same head again.
///
/// Only the `state` cells are read, and each must read: a state this build cannot read may name
/// a head, so it is an error, never a haul taken to propose nothing. Every other cell, and any
/// path outside a haul's known shape, names no head the haul merges, and is not read.
///
/// # Returns
/// * `Ok(Vec<String>)` - The proposed heads, in path order; empty while `@haul` is unborn.
/// * `Err(String)`     - If the `@haul` head could not be read, or a state cell could not be.
pub fn unmerged_haul_heads() -> Result<Vec<String>, String> {
    let Some(state) = meta_tree::read_meta_head(HAUL_PALLET_NAME, &HaulRules)? else {
        return Ok(Vec::new());
    };

    let mut heads = Vec::new();

    for (path, cell) in &state.cells {
        let Ok(CellPath { haul, name: CellName::State }) = parse_cell_path(path) else {
            continue;
        };

        let (status, head) = readable(path, cell, SCHEMA_STATE)
            .and_then(|envelope| parse_state(&envelope.value).map_err(|why| format!("the state of haul {} {}", short(haul), why)))
            .map_err(|why| format!("The proposed head of a haul could not be read: {}.", why))?;

        if !matches!(status, HaulStatus::Merged(_)) {
            heads.push(head);
        }
    }

    Ok(heads)
}

/// A short display form of a haul id.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// The cells of one haul directory, by name, while reading.
#[derive(Default)]
struct HaulCells {
    opened: Option<CellRead>,
    state: Option<CellRead>,
    reviews: Vec<(String, CellRead)>,
    comments: Vec<(String, CellRead)>,

    /// Cells under the haul whose name this build does not know.
    other: Vec<String>,
}

/// Read every haul at the local head, attributed through the office (read best-effort, so a
/// cell whose key the local office does not know is attributed by key id). An unborn pallet
/// reads as empty.
pub fn read_hauls() -> Result<HaulsRead, String> {
    let Some(state) = meta_tree::read_meta_head(HAUL_PALLET_NAME, &HaulRules)? else {
        return Ok(HaulsRead::default());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let author = |signer: &str| office.find_key(signer).map(|key| key.operator.clone()).unwrap_or(signer.to_string());

    Ok(hauls_in(state, &author))
}

/// Read every haul at `@haul` head `head`, each cell's signer named by `author`. For a reader with
/// no local pallet head to read from — a remote head judging a delete, whose objects sit in a
/// scratch mirror and which needs a haul's status, not who wrote it.
pub fn read_hauls_at(head: &str, author: &dyn Fn(&str) -> String) -> Result<HaulsRead, String> {
    Ok(hauls_in(meta_tree::read_meta_state(HAUL_PALLET_NAME, head, &HaulRules)?, author))
}

/// The hauls in `@haul` state `state`, each cell's signer named by `author`.
fn hauls_in(state: MetaState, author: &dyn Fn(&str) -> String) -> HaulsRead {
    let mut read = HaulsRead::default();
    let mut dirs: BTreeMap<String, HaulCells> = BTreeMap::new();

    for (path, cell) in state.cells {
        let parsed = match parse_cell_path(&path) {
            Ok(parsed) => parsed,
            Err(reason) => {
                read.stray.push(UnreadableCell { path, reason });
                continue;
            }
        };

        let cells = dirs.entry(parsed.haul.to_string()).or_default();

        match parsed.name {
            CellName::Opened => cells.opened = Some(cell),
            CellName::State => cells.state = Some(cell),
            CellName::Review { .. } => cells.reviews.push((path, cell)),
            CellName::Comment { .. } => cells.comments.push((path, cell)),
            CellName::Other => cells.other.push(path),
        }
    }

    for (id, cells) in dirs {
        match read_haul(&id, cells, author) {
            Ok(haul) => read.hauls.push(haul),
            Err(reason) => read.unreadable.push(UnreadableHaul { id, reason }),
        }
    }

    read.hauls.sort_by(|a, b| b.opened.opened_at.cmp(&a.opened.opened_at).then(b.id.cmp(&a.id)));

    read
}

/// Refuse a delete of `pallet` while an open haul in `hauls` names it as its source or target, or
/// while a haul cannot be read (it might). Returns the merged hauls whose target it is: their proof will
/// read as unproven once the target is gone.
pub fn refuse_an_open_haul(pallet: &str, hauls: &HaulsRead) -> Result<Vec<String>, String> {
    if let Some(unreadable) = hauls.unreadable.first() {
        return Err(format!(
            "Cannot tell whether haul {} names pallet \"{}\": {} Nothing was deleted.",
            unreadable.id, pallet, unreadable.reason
        ));
    }

    let mut merged = Vec::new();

    for haul in &hauls.hauls {
        let names = haul.opened.source == pallet || haul.opened.target == pallet;

        match haul.status {
            HaulStatus::Open if names => return Err(format!(
                "Open haul {} (\"{}\" into \"{}\") names pallet \"{}\"; merge or close the haul \
                before deleting the pallet.",
                haul.id, haul.opened.source, haul.opened.target, pallet
            )),
            HaulStatus::Merged(_) if haul.opened.target == pallet => merged.push(haul.id.clone()),
            _ => {}
        }
    }

    Ok(merged)
}

/// A cell's value, by the schema its name holds; an error naming why it cannot be read.
fn readable<'c>(path: &str, cell: &'c CellRead, schema: &str) -> Result<&'c CellEnvelope, String> {
    match cell {
        CellRead::Readable(envelope) if envelope.schema == schema => Ok(envelope),
        CellRead::Readable(envelope) => Err(format!("the cell {} holds a {} value", path, envelope.schema)),
        CellRead::Unknown { schema, .. } =>
            Err(format!("the cell {} is written with the schema \"{}\", which this forklift does not know", path, schema)),
        CellRead::Malformed(why) => Err(format!("the cell {} is malformed: {}", path, why)),
    }
}

/// Read one haul directory. Its proposal and state must both read, or nothing about the
/// haul does; a review or comment that cannot read is listed beside the rest.
fn read_haul(id: &str, cells: HaulCells, author: &dyn Fn(&str) -> String) -> Result<Haul, String> {
    let dir = haul_dir(id);
    let named = |name: &str| format!("{}/{}", id, name);

    let opened = cells.opened.as_ref().ok_or(format!("the haul has cells but no {} cell", CELL_OPENED))?;
    let opened = readable(&named(CELL_OPENED), opened, SCHEMA_OPENED)?;
    let opened = parse_opened(&opened.value).map_err(|why| format!("the proposal {}", why))?;

    let state = cells.state.as_ref().ok_or(format!("the haul has no {} cell", CELL_STATE))?;
    let (status, head) = parse_state(&readable(&named(CELL_STATE), state, SCHEMA_STATE)?.value)
        .map_err(|why| format!("the state {}", why))?;

    let mut haul = Haul {
        id: id.to_string(),
        opened,
        head,
        status,
        reviews: Vec::new(),
        comments: Vec::new(),
        unreadable: Vec::new(),
    };

    let display = |path: &str| path.strip_prefix(&format!("{}/", &dir[..2])).unwrap_or(path).to_string();

    for (path, cell) in &cells.reviews {
        let parsed = readable(&display(path), cell, SCHEMA_REVIEW).and_then(|envelope| {
            parse_review(&envelope.value)
                .map(|review| Review { author: author(&envelope.signer), ..review })
                .map_err(|why| format!("the review at {} {}", display(path), why))
        });

        match parsed {
            Ok(review) => haul.reviews.push(review),
            Err(reason) => haul.unreadable.push(UnreadableCell { path: display(path), reason }),
        }
    }

    for (path, cell) in &cells.comments {
        let parsed = readable(&display(path), cell, SCHEMA_COMMENT).and_then(|envelope| {
            parse_comment(&envelope.value)
                .map(|comment| Comment { author: author(&envelope.signer), ..comment })
                .map_err(|why| format!("the comment at {} {}", display(path), why))
        });

        match parsed {
            Ok(comment) => haul.comments.push(comment),
            Err(reason) => haul.unreadable.push(UnreadableCell { path: display(path), reason }),
        }
    }

    for path in &cells.other {
        haul.unreadable.push(UnreadableCell {
            path: display(path),
            reason: "a cell this forklift does not know a haul to hold".to_string(),
        });
    }

    haul.comments.sort_by_key(|comment| comment.commented_at);

    Ok(haul)
}

/// Find one haul by an id prefix (like a parcel hash prefix). Errors if none or several match,
/// or if the one that matches cannot be read.
pub fn find_haul(id_prefix: &str) -> Result<Haul, String> {
    let read = read_hauls()?;

    let unreadable: Vec<&UnreadableHaul> = read.unreadable.iter().filter(|haul| haul.id.starts_with(id_prefix)).collect();
    let mut matches: Vec<Haul> = read.hauls.into_iter().filter(|haul| haul.id.starts_with(id_prefix)).collect();

    match (matches.len(), unreadable.as_slice()) {
        (0, []) => Err(format!("No haul matches \"{}\".", id_prefix)),
        (1, []) => Ok(matches.remove(0)),
        (0, [haul]) => Err(format!("Haul {} cannot be read: {}.", short(&haul.id), haul.reason)),
        (n, more) => Err(format!("\"{}\" is ambiguous — it matches {} hauls.", id_prefix, n + more.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened() -> Opened {
        Opened {
            source: "feature".to_string(),
            target: "main".to_string(),
            title: "Add the thing".to_string(),
            description: "please\nreview".to_string(),
            opener: "alice".to_string(),
            opened_at: 100,
        }
    }

    #[test]
    fn every_value_round_trips() {
        assert_eq!(parse_opened(&opened_value(&opened())).unwrap(), opened());

        let head = "a".repeat(64);
        for status in [HaulStatus::Open, HaulStatus::Closed, HaulStatus::Merged("b".repeat(64))] {
            assert_eq!(parse_state(&state_value(&status, &head)).unwrap(), (status, head.clone()));
        }

        let review = parse_review(&review_value(ReviewVerdict::RequestChanges, "not yet", &head, 7)).unwrap();
        assert_eq!((review.verdict, review.body.as_str(), review.head.as_str(), review.reviewed_at),
                   (ReviewVerdict::RequestChanges, "not yet", head.as_str(), 7));

        let comment = parse_comment(&comment_value("hi", 9)).unwrap();
        assert_eq!((comment.body.as_str(), comment.commented_at), ("hi", 9));
    }

    #[test]
    fn values_are_parsed_strictly() {
        let mut extra = opened_value(&opened());
        extra.insert("recorded_at", value(1));
        assert!(parse_opened(&extra).unwrap_err().contains("unexpected field \"recorded_at\""));

        let head = "a".repeat(64);
        assert!(parse_state(&state_value(&HaulStatus::Open, "abc")).unwrap_err().contains("full parcel hash"));

        let mut headless = state_value(&HaulStatus::Merged("b".repeat(64)), &head);
        headless.remove(FIELD_HEAD);
        assert!(parse_state(&headless).is_err(), "every state names its head");

        let mut open_with_parcel = state_value(&HaulStatus::Open, &head);
        open_with_parcel.insert(FIELD_MERGE_PARCEL, value("b".repeat(64)));
        assert!(parse_state(&open_with_parcel).is_err(), "only a merged state names a parcel");

        let mut merged_without = state_value(&HaulStatus::Merged("b".repeat(64)), &head);
        merged_without.remove(FIELD_MERGE_PARCEL);
        assert!(parse_state(&merged_without).is_err(), "a merged state names its parcel");

        let mut unknown_state = state_value(&HaulStatus::Open, &head);
        unknown_state.insert(FIELD_STATE, value("draft"));
        assert!(parse_state(&unknown_state).unwrap_err().contains("\"draft\""));

        let mut bad_verdict = review_value(ReviewVerdict::Approve, "", &"a".repeat(64), 1);
        bad_verdict.insert(FIELD_VERDICT, value("lgtm"));
        assert!(parse_review(&bad_verdict).is_err());

        let mut unpinned = review_value(ReviewVerdict::Approve, "", &"a".repeat(64), 1);
        unpinned.remove(FIELD_HEAD);
        assert!(parse_review(&unpinned).unwrap_err().contains("head"), "a review names the head it reviewed");
    }

    #[test]
    fn the_haul_id_derives_from_every_field_of_the_proposal() {
        let base = opened().id();
        assert_eq!(base, opened().id());

        let mut variants = Vec::new();
        for change in 0..6 {
            let mut other = opened();
            match change {
                0 => other.source.push('x'),
                1 => other.target.push('x'),
                2 => other.title.push('x'),
                3 => other.description.push('x'),
                4 => other.opener = "bob".to_string(),
                _ => other.opened_at += 1,
            }
            variants.push(other.id());
        }

        assert!(variants.iter().all(|id| *id != base), "two hauls against one target never share an id");

        // Length prefixes: moving a character between fields changes the id.
        let mut shifted = opened();
        shifted.source = "featur".to_string();
        shifted.target = "emain".to_string();
        assert_ne!(shifted.id(), base);
    }

    #[test]
    fn a_cell_path_parses_only_at_its_shape() {
        let id = opened().id();
        let dir = haul_dir(&id);
        let hex = "e".repeat(64);

        assert_eq!(parse_cell_path(&format!("{}/opened", dir)).unwrap().name, CellName::Opened);
        assert_eq!(parse_cell_path(&format!("{}/review/{}", dir, hex)).unwrap().name, CellName::Review { owner: &hex });
        assert_eq!(
            parse_cell_path(&format!("{}/comment/{}/{}", dir, hex, hex)).unwrap().name,
            CellName::Comment { owner: &hex, id: &hex }
        );
        assert_eq!(parse_cell_path(&format!("{}/label", dir)).unwrap().name, CellName::Other);

        for wrong in [
            format!("{}/opened/extra", dir),
            format!("{}/review", dir),
            format!("{}/review/short", dir),
            format!("{}/comment/{}", dir, hex),
            format!("zz/{}/opened", id),
            format!("{}/opened", &id[..2]),
        ] {
            assert!(parse_cell_path(&wrong).is_err(), "{} must not parse", wrong);
        }
    }

    #[test]
    fn a_comment_id_separates_body_and_time() {
        assert_eq!(comment_id(&comment_value("a", 1)), comment_id(&comment_value("a", 1)));
        assert_ne!(comment_id(&comment_value("a", 1)), comment_id(&comment_value("a", 2)));
        assert_ne!(comment_id(&comment_value("a", 1)), comment_id(&comment_value("b", 1)));
    }
}
