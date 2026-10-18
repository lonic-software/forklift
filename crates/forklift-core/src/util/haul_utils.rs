//! Hauls: reviewable merge proposals (pull requests), on the meta-tree substrate (`meta_tree`).
//!
//! A *haul* proposes merging a source pallet into a target pallet, with discussion and signed
//! reviews. Each haul is a directory of cells on the `@haul` meta pallet, at `<h[0..2]>/<h>/` for
//! haul id `h`:
//!
//! - `opened` — the proposal: source, target, title, description, the opener's operator id and
//!   when it was opened. The haul id is derived from it, so two hauls opened against the same
//!   target never share an id. Never changed or removed.
//! - `head` — the proposed source head. Moves when the proposal is updated; never removed.
//! - `status` — open, closed or merged; absent means open. Once merged, never changed again.
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
    self, CellEdit, CellEnvelope, CellRead, MetaKind, MetaSigner, PalletRules, ParcelChange,
};
use crate::util::office_utils::{self, OfficeState};
use crate::util::object_utils;

/// The name of the haul meta pallet — reached as `@haul` (DESIGN.html §3.3).
pub const HAUL_PALLET_NAME: &str = "haul";

const CELL_OPENED: &str = "opened";
const CELL_HEAD: &str = "head";
const CELL_STATUS: &str = "status";
const DIR_REVIEW: &str = "review";
const DIR_COMMENT: &str = "comment";

pub(crate) const SCHEMA_OPENED: &str = "haul.opened 1";
pub(crate) const SCHEMA_HEAD: &str = "haul.head 1";
pub(crate) const SCHEMA_STATUS: &str = "haul.status 1";
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

/// A haul whose `opened`, `head` or `status` cell this build cannot read. Nothing about it is
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
    Head,
    Status,

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
        [CELL_HEAD] => CellName::Head,
        [CELL_STATUS] => CellName::Status,
        [DIR_REVIEW, owner] if hex(owner) => CellName::Review { owner },
        [DIR_COMMENT, owner, id] if hex(owner) && hex(id) => CellName::Comment { owner, id },
        [first, ..] if ![CELL_OPENED, CELL_HEAD, CELL_STATUS, DIR_REVIEW, DIR_COMMENT].contains(first) =>
            CellName::Other,
        _ => return Err(format!("{} is not at a shape its cell takes", path)),
    };

    Ok(CellPath { haul, name })
}

/// The schema a known cell name holds.
fn schema_of(name: &CellName) -> Option<&'static str> {
    match name {
        CellName::Opened => Some(SCHEMA_OPENED),
        CellName::Head => Some(SCHEMA_HEAD),
        CellName::Status => Some(SCHEMA_STATUS),
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

pub(crate) fn head_value(head: &str) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_HEAD, value(head));

    table
}

fn parse_head(table: &Table) -> Result<String, String> {
    meta_tree::value_fields_only(table, &[FIELD_HEAD])?;

    value_hash(table, FIELD_HEAD)
}

pub(crate) fn status_value(status: &HaulStatus) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_STATE, value(status.as_str()));

    if let HaulStatus::Merged(parcel) = status {
        table.insert(FIELD_MERGE_PARCEL, value(parcel.as_str()));
    }

    table
}

/// Parse a status strictly: a merge parcel exactly when merged.
fn parse_status(table: &Table) -> Result<HaulStatus, String> {
    match meta_tree::value_string(table, FIELD_STATE)?.as_str() {
        "merged" => {
            meta_tree::value_fields_only(table, &[FIELD_STATE, FIELD_MERGE_PARCEL])?;
            Ok(HaulStatus::Merged(value_hash(table, FIELD_MERGE_PARCEL)?))
        }
        state @ ("open" | "closed") => {
            meta_tree::value_fields_only(table, &[FIELD_STATE])?;
            Ok(if state == "open" { HaulStatus::Open } else { HaulStatus::Closed })
        }
        other => Err(format!("has the state \"{}\", which is not open, closed or merged", other)),
    }
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
        CellName::Head => parse_head(table).map(|_| ()),
        CellName::Status => parse_status(table).map(|_| ()),
        CellName::Review { .. } => parse_review(table).map(|_| ()),
        CellName::Comment { .. } => parse_comment(table).map(|_| ()),
        CellName::Other => Ok(()),
    }
}

/// The pallet rules of `@haul`:
///
/// - `opened` is written once, by its opener, at the id its content derives, together with a
///   `head`, and is never changed or removed;
/// - a haul directory holds no other cell unless its `opened` cell is present;
/// - `head` is never removed;
/// - a `status` that reads merged is never changed or removed, and a status this build cannot
///   read is never changed either, since it might read merged;
/// - `review/<op>` and `comment/<op>/…` are written or removed only by the operator `op` names;
/// - every cell holds its own schema and a strictly valid value, and no cell this build does
///   not know is written.
pub struct HaulRules;

impl PalletRules for HaulRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Haul
    }

    fn knows_schema(&self, schema: &str) -> bool {
        [SCHEMA_OPENED, SCHEMA_HEAD, SCHEMA_STATUS, SCHEMA_REVIEW, SCHEMA_COMMENT].contains(&schema)
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        let operator = &change.signer.operator;
        let owner = operator_leaf(operator);

        for cell in change.changes {
            let path = parse_cell_path(&cell.path)?;
            let dir = haul_dir(path.haul);

            match &path.name {
                CellName::Opened if cell.before.is_some() => return Err(format!(
                    "{} is the proposal of a haul, which is never changed or removed", cell.path
                )),
                CellName::Head if cell.after.is_none() => return Err(format!(
                    "{} is the head of a haul, which is never removed", cell.path
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

            if path.name == CellName::Status {
                if let Some(before) = &cell.before {
                    let merged = match meta_tree::read_cell_blob(before, self)? {
                        CellRead::Readable(envelope) => parse_status(&envelope.value)
                            .map(|status| matches!(status, HaulStatus::Merged(_)))
                            .map_err(|why| format!("the status at {} {}", cell.path, why))?,
                        _ => return Err(format!(
                            "the status at {} cannot be read by this forklift, so it cannot tell \
                            whether the haul was merged; upgrade to change it",
                            cell.path
                        )),
                    };

                    if merged {
                        return Err(format!("the haul {} is merged, and a merge is final", path.haul));
                    }
                }
            }

            let Some(after) = &cell.after else { continue };

            if meta_tree::read_cell_in(change.cells, &format!("{}/{}", dir, CELL_OPENED), self)?.is_none() {
                return Err(format!(
                    "the cell at {} belongs to a haul with no proposal; resolve the whole haul \
                    (its id) rather than its proposal alone",
                    cell.path
                ));
            }

            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;
            let Some(schema) = schema_of(&path.name) else { continue };


            if envelope.schema != schema {
                // A newer schema at a known cell is carried, except at the proposal, which the
                // rules must read to check its id and its opener.
                if self.knows_schema(&envelope.schema) || path.name == CellName::Opened {
                    return Err(format!("the cell at {} holds a {} value", cell.path, envelope.schema));
                }

                continue;
            }

            check_value(&path.name, &envelope.value).map_err(|why| format!("the value at {} {}", cell.path, why))?;

            match &path.name {
                CellName::Opened => {
                    let opened = parse_opened(&envelope.value)?;

                    if opened.id() != path.haul {
                        return Err(format!("the proposal at {} is not at the id its content derives", cell.path));
                    }

                    if &opened.opener != operator {
                        return Err(format!(
                            "the proposal at {} names {} as its opener, but is signed by {}",
                            cell.path, opened.opener, operator
                        ));
                    }

                    if meta_tree::read_cell_in(change.cells, &format!("{}/{}", dir, CELL_HEAD), self)?.is_none() {
                        return Err(format!("the haul {} is opened without a head", path.haul));
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
            SCHEMA_HEAD => parse_head(table).map(|head| format!("head {}", head)),
            SCHEMA_STATUS => parse_status(table).map(|status| match status {
                HaulStatus::Merged(parcel) => format!("merged as {}", parcel),
                status => status.as_str().to_string(),
            }),
            SCHEMA_REVIEW => parse_review(table).map(|review| format!("{} of {}", review.verdict.as_str(), review.head)),
            SCHEMA_COMMENT => parse_comment(table).map(|comment| comment.body),
            _ => Ok(table.to_string()),
        };

        shown.unwrap_or_else(|why| format!("(unreadable: the value {})", why))
    }
}

/// Stack edits to the local `@haul` head as one signed parcel. `None` when they changed nothing.
fn stack(edits: &[CellEdit],
         office: &OfficeState,
         actor: &Operator,
         description: &str,
         signer: &MetaSigner) -> Result<Option<String>, String> {
    meta_tree::stack_meta_edits(HAUL_PALLET_NAME, &HaulRules, office, edits, actor, description, signer)
}

/// The operator a signing key belongs to.
fn signing_operator(office: &OfficeState, signer: &MetaSigner) -> Result<String, String> {
    Ok(office.find_key(&signer.key_id)
        .ok_or(format!("The signing key {} is not in the office.", signer.key_id))?
        .operator
        .clone())
}

/// Open a haul: its proposal and its head, as one signed parcel. The caller holds the warehouse
/// lock and passes the verified office. Returns the new haul id.
pub fn open_haul(source: &str,
                 target: &str,
                 head: &str,
                 title: &str,
                 description: &str,
                 office: &OfficeState,
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
        meta_tree::set_cell(&format!("{}/{}", dir, CELL_HEAD), &signer.key_id, SCHEMA_HEAD, &head_value(head))?,
    ];

    stack(&edits, office, actor, &format!("Opened haul \"{}\".", title), signer)?
        .ok_or(format!("Haul {} is already open.", short(&id)))?;

    Ok(id)
}

/// Record a comment on a haul.
pub fn record_comment(haul_id: &str,
                      body: &str,
                      office: &OfficeState,
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
                     office: &OfficeState,
                     actor: &Operator,
                     signer: &MetaSigner) -> Result<Option<String>, String> {
    let path = format!("{}/{}/{}", haul_dir(haul_id), DIR_REVIEW, operator_leaf(&signing_operator(office, signer)?));
    let review = review_value(verdict, body, head, Utc::now().timestamp());
    let edit = meta_tree::set_cell(&path, &signer.key_id, SCHEMA_REVIEW, &review)?;

    stack(&[edit], office, actor, &format!("Reviewed haul {} ({}).", short(haul_id), verdict.as_str()), signer)
}

/// Record a new proposed head. `None` when it is already the head.
pub fn record_head(haul_id: &str,
                   head: &str,
                   office: &OfficeState,
                   actor: &Operator,
                   signer: &MetaSigner) -> Result<Option<String>, String> {
    let edit = meta_tree::set_cell(&format!("{}/{}", haul_dir(haul_id), CELL_HEAD), &signer.key_id, SCHEMA_HEAD, &head_value(head))?;

    stack(&[edit], office, actor, &format!("Updated haul {}.", short(haul_id)), signer)
}

/// Record a haul's status: closed, reopened, or merged as a parcel on the target.
pub fn record_status(haul_id: &str,
                     status: &HaulStatus,
                     office: &OfficeState,
                     actor: &Operator,
                     signer: &MetaSigner) -> Result<Option<String>, String> {
    let path = format!("{}/{}", haul_dir(haul_id), CELL_STATUS);
    let edit = meta_tree::set_cell(&path, &signer.key_id, SCHEMA_STATUS, &status_value(status))?;

    let verb = match status {
        HaulStatus::Open => "Reopened",
        HaulStatus::Closed => "Closed",
        HaulStatus::Merged(_) => "Merged",
    };

    stack(&[edit], office, actor, &format!("{} haul {}.", verb, short(haul_id)), signer)
}

/// A short display form of a haul id.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// The cells of one haul directory, by name, while reading.
#[derive(Default)]
struct HaulCells {
    opened: Option<CellRead>,
    head: Option<CellRead>,
    status: Option<CellRead>,
    reviews: Vec<(String, CellRead)>,
    comments: Vec<(String, CellRead)>,
    other: Vec<(String, CellRead)>,
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
            CellName::Head => cells.head = Some(cell),
            CellName::Status => cells.status = Some(cell),
            CellName::Review { .. } => cells.reviews.push((path, cell)),
            CellName::Comment { .. } => cells.comments.push((path, cell)),
            CellName::Other => cells.other.push((path, cell)),
        }
    }

    let author = |signer: &str| office.find_key(signer).map(|key| key.operator.clone()).unwrap_or(signer.to_string());

    for (id, cells) in dirs {
        match read_haul(&id, cells, &author) {
            Ok(haul) => read.hauls.push(haul),
            Err(reason) => read.unreadable.push(UnreadableHaul { id, reason }),
        }
    }

    read.hauls.sort_by(|a, b| b.opened.opened_at.cmp(&a.opened.opened_at).then(b.id.cmp(&a.id)));

    Ok(read)
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

/// Read one haul directory. Its proposal, head and status must all read, or nothing about the
/// haul does; a review or comment that cannot read is listed beside the rest.
fn read_haul(id: &str, cells: HaulCells, author: &dyn Fn(&str) -> String) -> Result<Haul, String> {
    let dir = haul_dir(id);
    let named = |name: &str| format!("{}/{}", id, name);

    let opened = cells.opened.as_ref().ok_or(format!("the haul has cells but no {} cell", CELL_OPENED))?;
    let opened = readable(&named(CELL_OPENED), opened, SCHEMA_OPENED)?;
    let opened = parse_opened(&opened.value).map_err(|why| format!("the proposal {}", why))?;

    let head = cells.head.as_ref().ok_or(format!("the haul has no {} cell", CELL_HEAD))?;
    let head = parse_head(&readable(&named(CELL_HEAD), head, SCHEMA_HEAD)?.value)
        .map_err(|why| format!("the head {}", why))?;

    let status = match &cells.status {
        Some(status) => parse_status(&readable(&named(CELL_STATUS), status, SCHEMA_STATUS)?.value)
            .map_err(|why| format!("the status {}", why))?,
        None => HaulStatus::Open,
    };

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

    for (path, _) in &cells.other {
        haul.unreadable.push(UnreadableCell {
            path: display(path),
            reason: "a cell this forklift does not know".to_string(),
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
        assert_eq!(parse_head(&head_value(&head)).unwrap(), head);

        for status in [HaulStatus::Open, HaulStatus::Closed, HaulStatus::Merged("b".repeat(64))] {
            assert_eq!(parse_status(&status_value(&status)).unwrap(), status);
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

        assert!(parse_head(&head_value("abc")).unwrap_err().contains("full parcel hash"));

        let mut open_with_parcel = status_value(&HaulStatus::Open);
        open_with_parcel.insert(FIELD_MERGE_PARCEL, value("b".repeat(64)));
        assert!(parse_status(&open_with_parcel).is_err(), "only a merged status names a parcel");

        let mut merged_without = status_value(&HaulStatus::Merged("b".repeat(64)));
        merged_without.remove(FIELD_MERGE_PARCEL);
        assert!(parse_status(&merged_without).is_err(), "a merged status names its parcel");

        let mut unknown_state = status_value(&HaulStatus::Open);
        unknown_state.insert(FIELD_STATE, value("draft"));
        assert!(parse_status(&unknown_state).unwrap_err().contains("\"draft\""));

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
