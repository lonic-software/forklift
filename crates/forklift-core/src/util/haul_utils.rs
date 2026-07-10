//! Hauls: reviewable merge proposals (pull requests), as tracked metadata.
//!
//! A *haul* proposes merging a source pallet into a target pallet, with discussion and
//! signed reviews. It is built on the exact substrate the manifest proved out: a
//! dedicated `@haul` meta pallet whose parcels each carry **one signed event**, so a
//! haul's whole life is an append-only log — Opened, Pushed, Comment, Review, Merged,
//! Closed, Reopened — and its current state is the *fold* of that log.
//!
//! Because authorship is the parcel's **signature** (never a stored field), "who approved
//! this" is forge-proof and carries the operator's §7.1 identity class — a human's
//! approval is distinguishable from an agent's. The ordinary `verify_pallet_history` is
//! the whole verification (no server special-case), and two diverged `@haul` heads merge
//! with a plain two-parent join: the union of independent events, never a conflict.
//!
//! MVP scope (approved 2026-07-05): intra-warehouse (pallet → pallet); reviews are
//! recorded but merging is not gated on them; a haul id is the content-addressed id of its
//! Opened event (shown as a prefix).

use std::collections::{BTreeMap, HashMap, HashSet};
use chrono::Utc;
use toml_edit::{value, DocumentMut};
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::blob::Blob;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::model::tree_item::TreeItem;
use crate::util::{object_utils, office_utils, pallet_utils, sign_utils};

/// The name of the haul meta pallet — reached as `@haul` (DESIGN.html §3.3).
pub const HAUL_PALLET_NAME: &str = "haul";

/// The tree namespace of haul events: `.forklift/tracked/haul/…` (shares the tracked root
/// with the office and manifest, so it inherits their collision-proofing).
const TREE_NAME_FORKLIFT: &str = ".forklift";
const TREE_NAME_TRACKED: &str = "tracked";
const TREE_NAME_HAUL: &str = "haul";
const RECORD_SUFFIX: &str = ".toml";

/// The kind of a haul event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HaulEventKind {
    /// Genesis: the proposal itself. Its content id is the haul id.
    Opened,
    /// The source pallet advanced — a new proposed head.
    Pushed,
    /// A discussion message.
    Comment,
    /// A review: approve, request changes, or a review comment.
    Review,
    /// The haul was merged (records the resulting merge parcel on the target).
    Merged,
    /// Closed without merging.
    Closed,
    /// Reopened after closing.
    Reopened,
}

impl HaulEventKind {
    /// The wire value of the kind.
    pub fn as_str(self) -> &'static str {
        match self {
            HaulEventKind::Opened => "opened",
            HaulEventKind::Pushed => "pushed",
            HaulEventKind::Comment => "comment",
            HaulEventKind::Review => "review",
            HaulEventKind::Merged => "merged",
            HaulEventKind::Closed => "closed",
            HaulEventKind::Reopened => "reopened",
        }
    }

    /// Parse a kind from its wire value.
    pub fn parse(value: &str) -> Result<HaulEventKind, String> {
        match value {
            "opened" => Ok(HaulEventKind::Opened),
            "pushed" => Ok(HaulEventKind::Pushed),
            "comment" => Ok(HaulEventKind::Comment),
            "review" => Ok(HaulEventKind::Review),
            "merged" => Ok(HaulEventKind::Merged),
            "closed" => Ok(HaulEventKind::Closed),
            "reopened" => Ok(HaulEventKind::Reopened),
            other => Err(format!("\"{}\" is not a haul event kind.", other)),
        }
    }
}

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

/// One event on the `@haul` log. Optional fields are present only for the kinds that use
/// them; the whole event is content-addressed by [`HaulEvent::id`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HaulEvent {
    /// The haul this event belongs to (the Opened event's id). Empty on the Opened event
    /// itself — its own id *is* the haul id.
    pub haul: String,

    pub kind: HaulEventKind,

    /// When the event was recorded. Display metadata only — never a security input.
    pub recorded_at: i64,

    /// Free text (a comment/review body, or the description on Opened). May be empty.
    pub body: String,

    /// Opened: the source pallet (wire ref).
    pub source: Option<String>,

    /// Opened: the target pallet (wire ref).
    pub target: Option<String>,

    /// Opened: the title.
    pub title: Option<String>,

    /// Opened / Pushed: the proposed source head.
    pub head: Option<String>,

    /// Review: the verdict.
    pub verdict: Option<ReviewVerdict>,

    /// Merged: the merge parcel created on the target.
    pub merge_parcel: Option<String>,
}

impl HaulEvent {
    /// The event's content-derived id — the filename it is stored under, and (for an
    /// Opened event) the haul id.
    pub fn id(&self) -> String {
        let material = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.haul,
            self.kind.as_str(),
            self.recorded_at,
            self.body,
            self.source.as_deref().unwrap_or(""),
            self.target.as_deref().unwrap_or(""),
            self.title.as_deref().unwrap_or(""),
            self.head.as_deref().unwrap_or(""),
            self.verdict.map(|v| v.as_str()).unwrap_or(""),
            self.merge_parcel.as_deref().unwrap_or(""),
        );

        blake3::hash(material.as_bytes()).to_hex().to_string()
    }
}

/// An event together with its forge-proof authorship.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedEvent {
    pub event: HaulEvent,

    /// The operator resolved from the introducing parcel's signature.
    pub author: String,

    /// The `@haul` parcel that introduced the event.
    pub parcel: String,
}

/// A review, folded to the latest verdict per author.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Review {
    pub author: String,
    pub verdict: ReviewVerdict,
    pub body: String,
    pub at: i64,
}

/// The status of a haul (the fold of its close/reopen/merge events).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HaulStatus {
    Open,
    Merged(String),
    Closed,
}

/// A haul: the folded state of one proposal's event log.
#[derive(Clone)]
pub struct Haul {
    pub id: String,
    pub source: String,
    pub target: String,
    pub title: String,
    pub description: String,

    /// The current proposed head (latest Pushed, else the Opened head).
    pub head: String,

    pub status: HaulStatus,
    pub opened_by: String,
    pub opened_at: i64,

    /// The latest review per author.
    pub reviews: Vec<Review>,

    /// Comments and reviews in the order they were recorded.
    pub thread: Vec<AttributedEvent>,
}

/// Read and fold every haul reachable from the `@haul` head, newest first (by open time).
///
/// # Returns
/// * `Ok(Vec<Haul>)` - The hauls (empty when the pallet is unborn).
/// * `Err(String)`   - If an object could not be read.
pub fn read_hauls() -> Result<Vec<Haul>, String> {
    let events = read_events()?;

    // Group events by haul id: the Opened event's own id, or the referenced id otherwise.
    let mut groups: HashMap<String, Vec<AttributedEvent>> = HashMap::new();

    for attributed in events {
        let id = if attributed.event.kind == HaulEventKind::Opened {
            attributed.event.id()
        } else {
            attributed.event.haul.clone()
        };

        groups.entry(id).or_default().push(attributed);
    }

    let mut hauls: Vec<Haul> = groups.into_iter()
        .filter_map(|(id, events)| fold_haul(id, events))
        .collect();

    hauls.sort_by(|a, b| b.opened_at.cmp(&a.opened_at).then(b.id.cmp(&a.id)));

    Ok(hauls)
}

/// Find one haul by an id prefix (like a parcel hash prefix). Errors if none or several
/// match.
pub fn find_haul(id_prefix: &str) -> Result<Haul, String> {
    let matches: Vec<Haul> = read_hauls()?
        .into_iter()
        .filter(|haul| haul.id.starts_with(id_prefix))
        .collect();

    match matches.len() {
        0 => Err(format!("No haul matches \"{}\".", id_prefix)),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => Err(format!("\"{}\" is ambiguous — it matches {} hauls.", id_prefix, n)),
    }
}

/// Fold one haul's event log into its current state. `None` when there is no Opened event
/// (a dangling reference).
fn fold_haul(id: String, mut events: Vec<AttributedEvent>) -> Option<Haul> {
    events.sort_by(|a, b| a.event.recorded_at.cmp(&b.event.recorded_at).then(a.parcel.cmp(&b.parcel)));

    let opened = events.iter().find(|e| e.event.kind == HaulEventKind::Opened)?;

    let mut haul = Haul {
        id,
        source: opened.event.source.clone().unwrap_or_default(),
        target: opened.event.target.clone().unwrap_or_default(),
        title: opened.event.title.clone().unwrap_or_default(),
        description: opened.event.body.clone(),
        head: opened.event.head.clone().unwrap_or_default(),
        status: HaulStatus::Open,
        opened_by: opened.author.clone(),
        opened_at: opened.event.recorded_at,
        reviews: Vec::new(),
        thread: Vec::new(),
    };

    let mut merged: Option<String> = None;
    let mut closed = false;
    let mut latest_review: BTreeMap<String, Review> = BTreeMap::new();

    for attributed in &events {
        match attributed.event.kind {
            HaulEventKind::Pushed => {
                if let Some(head) = &attributed.event.head {
                    haul.head = head.clone();
                }
            }
            HaulEventKind::Merged => merged = attributed.event.merge_parcel.clone(),
            HaulEventKind::Closed => closed = true,
            HaulEventKind::Reopened => closed = false,
            HaulEventKind::Comment => haul.thread.push(attributed.clone()),
            HaulEventKind::Review => {
                if let Some(verdict) = attributed.event.verdict {
                    latest_review.insert(attributed.author.clone(), Review {
                        author: attributed.author.clone(),
                        verdict,
                        body: attributed.event.body.clone(),
                        at: attributed.event.recorded_at,
                    });
                }
                haul.thread.push(attributed.clone());
            }
            HaulEventKind::Opened => {}
        }
    }

    // A merge is final; otherwise the latest close/reopen decides.
    haul.status = match merged {
        Some(parcel) => HaulStatus::Merged(parcel),
        None if closed => HaulStatus::Closed,
        None => HaulStatus::Open,
    };
    haul.reviews = latest_review.into_values().collect();

    Some(haul)
}

/// Open a haul: stack the genesis (Opened) event and return the new haul id.
pub fn open_haul(source: &str,
                 target: &str,
                 head: &str,
                 title: &str,
                 description: &str,
                 actor: &Operator,
                 signing_key_id: &str) -> Result<String, String> {
    let event = HaulEvent {
        haul: String::new(),
        kind: HaulEventKind::Opened,
        recorded_at: Utc::now().timestamp(),
        body: description.to_string(),
        source: Some(source.to_string()),
        target: Some(target.to_string()),
        title: Some(title.to_string()),
        head: Some(head.to_string()),
        verdict: None,
        merge_parcel: None,
    };

    let id = event.id();
    record_event(&event, actor, format!("Opened haul \"{}\".", title), signing_key_id)?;

    Ok(id)
}

/// Record a comment on a haul.
pub fn record_comment(haul_id: &str, body: &str, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    record_event(&event(haul_id, HaulEventKind::Comment, body), actor,
        format!("Commented on haul {}.", short(haul_id)), signing_key_id)
}

/// Record a review on a haul.
pub fn record_review(haul_id: &str, verdict: ReviewVerdict, body: &str, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    let mut event = event(haul_id, HaulEventKind::Review, body);
    event.verdict = Some(verdict);
    record_event(&event, actor, format!("Reviewed haul {} ({}).", short(haul_id), verdict.as_str()), signing_key_id)
}

/// Record that the source advanced to a new head.
pub fn record_pushed(haul_id: &str, head: &str, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    let mut event = event(haul_id, HaulEventKind::Pushed, "");
    event.head = Some(head.to_string());
    record_event(&event, actor, format!("Updated haul {}.", short(haul_id)), signing_key_id)
}

/// Record that the haul was merged (the merge parcel is on the target).
pub fn record_merged(haul_id: &str, merge_parcel: &str, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    let mut event = event(haul_id, HaulEventKind::Merged, "");
    event.merge_parcel = Some(merge_parcel.to_string());
    record_event(&event, actor, format!("Merged haul {}.", short(haul_id)), signing_key_id)
}

/// Record that the haul was closed or reopened.
pub fn record_closed(haul_id: &str, closed: bool, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    let kind = if closed { HaulEventKind::Closed } else { HaulEventKind::Reopened };
    let verb = if closed { "Closed" } else { "Reopened" };
    record_event(&event(haul_id, kind, ""), actor, format!("{} haul {}.", verb, short(haul_id)), signing_key_id)
}

/// Merge another `@haul` head into the local one: a two-parent join (union of events).
pub fn merge_hauls(other_head: &str, actor: &Operator, description: String, signing_key_id: &str) -> Result<String, String> {
    let local_head = pallet_utils::get_meta_pallet_head(HAUL_PALLET_NAME)?
        .ok_or("There is no local haul log to merge into.".to_string())?;

    stack_haul_parcel(None, vec![local_head, other_head.to_string()], actor, description, signing_key_id)
}

/// Build a bare event referencing a haul.
fn event(haul_id: &str, kind: HaulEventKind, body: &str) -> HaulEvent {
    HaulEvent {
        haul: haul_id.to_string(),
        kind,
        recorded_at: Utc::now().timestamp(),
        body: body.to_string(),
        source: None,
        target: None,
        title: None,
        head: None,
        verdict: None,
        merge_parcel: None,
    }
}

/// A short display form of a haul id.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Read every event reachable from the `@haul` head, each attributed to its signer.
fn read_events() -> Result<Vec<AttributedEvent>, String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(HAUL_PALLET_NAME)? else {
        return Ok(Vec::new());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(office_utils::OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut collected: Vec<AttributedEvent> = Vec::new();
    let mut deduped: HashSet<(String, String)> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![head];

    while let Some(hash) = queue.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        queue.extend(parcel.parents.clone());

        let Some(haul_tree) = resolve_subtree(&parcel.tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_HAUL])? else {
            continue; // A join parcel: no event.
        };

        let author = resolve_author(&hash, &office);

        for (_, file) in haul_tree.get_files() {
            let event = parse_event(&load_record(&file.hash)?)?;

            if deduped.insert((author.clone(), event.id())) {
                collected.push(AttributedEvent { event, author: author.clone(), parcel: hash.clone() });
            }
        }
    }

    Ok(collected)
}

/// The operator who signed a haul parcel.
fn resolve_author(parcel_hash: &str, office: &office_utils::OfficeState) -> String {
    match sign_utils::load_parcel_signature(parcel_hash) {
        Ok(Some(signature)) => office.find_key(&signature.key_id)
            .map(|key| key.operator.clone())
            .unwrap_or(signature.key_id),
        _ => "unsigned".to_string(),
    }
}

/// Record a new haul event: stack a single-event parcel signed by the actor onto the
/// `@haul` head.
fn record_event(event: &HaulEvent, actor: &Operator, description: String, signing_key_id: &str) -> Result<String, String> {
    let parents: Vec<String> = pallet_utils::get_meta_pallet_head(HAUL_PALLET_NAME)?
        .into_iter()
        .collect();

    let file_name = format!("{}{}", event.id(), RECORD_SUFFIX);
    let blob_hash = store_record(&event_to_toml(event))?;

    let mut haul_tree = TreeItem::new(TREE_NAME_HAUL.to_string(), String::new(), DirEntryType::Tree);
    haul_tree.add_child(TreeItem::new(file_name, blob_hash, DirEntryType::Normal));

    stack_haul_parcel(Some(haul_tree), parents, actor, description, signing_key_id)
}

/// Build, sign and store a haul parcel with the given (optional) event subtree and
/// parents, and advance the `@haul` head. `event_subtree` is `None` for a join parcel.
fn stack_haul_parcel(event_subtree: Option<TreeItem>,
                     parents: Vec<String>,
                     actor: &Operator,
                     description: String,
                     signing_key_id: &str) -> Result<String, String> {
    let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

    if let Some(mut haul_tree) = event_subtree {
        store_subtree(&mut haul_tree)?;

        let mut tracked_tree = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
        tracked_tree.add_child(haul_tree);
        store_subtree(&mut tracked_tree)?;

        let mut forklift_tree = TreeItem::new(TREE_NAME_FORKLIFT.to_string(), String::new(), DirEntryType::Tree);
        forklift_tree.add_child(tracked_tree);
        store_subtree(&mut forklift_tree)?;

        root_tree.add_child(forklift_tree);
    }

    let mut root_object = LooseObjectBuilder::build_tree(&root_tree);
    root_object.store()?;

    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash: root_object.hash.clone(),
        parents,
        actions: vec![
            ParcelAction { operator: actor.clone(), action: ParcelActionType::Author, description: None, timestamp },
            ParcelAction { operator: actor.clone(), action: ParcelActionType::Stack, description: None, timestamp },
        ],
        description: Some(description),
    };

    let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
    parcel_object.store()?;

    let signature = sign_utils::sign_parcel_hash(signing_key_id, &parcel_object.hash)?;
    sign_utils::store_parcel_signature(&parcel_object.hash, &signature)?;

    pallet_utils::set_meta_pallet_head(HAUL_PALLET_NAME, &parcel_object.hash)?;

    Ok(parcel_object.hash)
}

/// Build and store one subtree's object, recording the hash on the tree item.
fn store_subtree(tree: &mut TreeItem) -> Result<(), String> {
    let mut object = LooseObjectBuilder::build_tree(tree);
    tree.hash = object.hash.clone();
    object.store()?;

    Ok(())
}

/// Store a record blob and return its hash.
fn store_record(toml: &str) -> Result<String, String> {
    let blob = Blob { content: toml.as_bytes().to_vec() };
    let mut object = LooseObjectBuilder::build_blob(&blob);
    object.store()?;

    Ok(object.hash)
}

/// Load one record blob as a string.
fn load_record(hash: &str) -> Result<String, String> {
    String::from_utf8(object_utils::load_blob(hash)?.content)
        .map_err(|_| format!("The haul record {} is not valid UTF-8.", hash))
}

/// Resolve a chain of subtree names from a root tree.
fn resolve_subtree(root_tree_hash: &str, path: &[&str]) -> Result<Option<TreeItem>, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for component in path {
        let subtree_hash = current.get_subtrees()
            .find(|(name, _)| name == component)
            .map(|(_, item)| item.hash.clone());

        match subtree_hash {
            Some(hash) => current = object_utils::load_tree(&hash)?,
            None => return Ok(None),
        }
    }

    Ok(Some(current))
}

/// Serialize a haul event as TOML.
fn event_to_toml(event: &HaulEvent) -> String {
    let mut doc = DocumentMut::new();

    doc["haul"] = value(event.haul.as_str());
    doc["kind"] = value(event.kind.as_str());
    doc["recorded_at"] = value(event.recorded_at);
    doc["body"] = value(event.body.as_str());

    if let Some(source) = &event.source { doc["source"] = value(source.as_str()); }
    if let Some(target) = &event.target { doc["target"] = value(target.as_str()); }
    if let Some(title) = &event.title { doc["title"] = value(title.as_str()); }
    if let Some(head) = &event.head { doc["head"] = value(head.as_str()); }
    if let Some(verdict) = event.verdict { doc["verdict"] = value(verdict.as_str()); }
    if let Some(merge_parcel) = &event.merge_parcel { doc["merge_parcel"] = value(merge_parcel.as_str()); }

    doc.to_string()
}

/// Parse a haul event from TOML.
fn parse_event(toml: &str) -> Result<HaulEvent, String> {
    let doc: DocumentMut = toml.parse()
        .map_err(|e| format!("A haul event is not valid TOML: {}", e))?;

    let read_string = |field: &str| -> Result<String, String> {
        doc.get(field)
            .and_then(|item| item.as_str())
            .map(|s| s.to_string())
            .ok_or(format!("A haul event has no \"{}\" field.", field))
    };

    let optional_string = |field: &str| -> Option<String> {
        doc.get(field).and_then(|item| item.as_str()).map(|s| s.to_string())
    };

    let verdict = match optional_string("verdict") {
        Some(value) => Some(ReviewVerdict::parse(&value)?),
        None => None,
    };

    Ok(HaulEvent {
        haul: read_string("haul")?,
        kind: HaulEventKind::parse(&read_string("kind")?)?,
        recorded_at: doc.get("recorded_at")
            .and_then(|item| item.as_integer())
            .ok_or("A haul event has no \"recorded_at\" field.".to_string())?,
        body: read_string("body")?,
        source: optional_string("source"),
        target: optional_string("target"),
        title: optional_string("title"),
        head: optional_string("head"),
        verdict,
        merge_parcel: optional_string("merge_parcel"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_toml() {
        let opened = HaulEvent {
            haul: String::new(),
            kind: HaulEventKind::Opened,
            recorded_at: 100,
            body: "please review".to_string(),
            source: Some("feature".to_string()),
            target: Some("main".to_string()),
            title: Some("Add the thing".to_string()),
            head: Some("a".repeat(64)),
            verdict: None,
            merge_parcel: None,
        };
        assert_eq!(parse_event(&event_to_toml(&opened)).unwrap(), opened);

        let review = HaulEvent {
            haul: opened.id(),
            kind: HaulEventKind::Review,
            recorded_at: 200,
            body: "looks good".to_string(),
            source: None,
            target: None,
            title: None,
            head: None,
            verdict: Some(ReviewVerdict::Approve),
            merge_parcel: None,
        };
        assert_eq!(parse_event(&event_to_toml(&review)).unwrap(), review);
    }

    #[test]
    fn fold_derives_status_head_and_latest_review() {
        let opened = HaulEvent {
            haul: String::new(), kind: HaulEventKind::Opened, recorded_at: 1,
            body: "d".to_string(), source: Some("feat".to_string()), target: Some("main".to_string()),
            title: Some("t".to_string()), head: Some("h1".to_string()), verdict: None, merge_parcel: None,
        };
        let id = opened.id();
        let attributed = |event: HaulEvent, author: &str, parcel: &str| AttributedEvent {
            event, author: author.to_string(), parcel: parcel.to_string(),
        };
        let mut pushed = event(&id, HaulEventKind::Pushed, "");
        pushed.recorded_at = 3;
        pushed.head = Some("h2".to_string());

        let mut approve = event(&id, HaulEventKind::Review, "ok");
        approve.recorded_at = 2;
        approve.verdict = Some(ReviewVerdict::Approve);

        let events = vec![
            attributed(opened, "alice", "p1"),
            attributed(approve, "bob", "p2"),
            attributed(pushed, "alice", "p3"),
        ];

        let haul = fold_haul(id.clone(), events).unwrap();
        assert_eq!(haul.head, "h2");                       // latest Pushed wins
        assert_eq!(haul.status, HaulStatus::Open);
        assert_eq!(haul.reviews.len(), 1);
        assert_eq!(haul.reviews[0].verdict, ReviewVerdict::Approve);
        assert_eq!(haul.opened_by, "alice");
    }
}
