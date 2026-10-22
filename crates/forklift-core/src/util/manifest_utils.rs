//! The manifest: post-metadata about parcels, on the meta-tree substrate (`meta_tree`).
//!
//! A *manifest entry* is a signed statement attached to a parcel after the fact — an
//! approval, a review note, machine-authorship provenance (§7.2) or a delivery record
//! (§7.3). An entry *references* its subject parcel and never mutates it (the §4.4
//! immutability invariant).
//!
//! Cells sit under `<s[0..2]>/<s>/<kind>/<op>` for subject parcel `s` and writer `op`, the
//! blake3 of the writer's operator identifier. An approval is single-valued per operator and is
//! the cell `approval/<op>` itself. Every other kind is multi-valued and lives at
//! `<kind>/<op>/<id>`, where `id` is derived from the entry, so identical entries from one writer
//! collapse and the same text from two writers never shares a cell. Who wrote an entry is its
//! cell's signer stamp; the pallet rule below lets a parcel write or remove cells only under its
//! own signer's `<op>`, whatever schema a cell carries, so no writer can occupy another's path.

use toml_edit::{value, Table};
use crate::model::operator::Operator;
use crate::util::meta_tree::{
    self, CellRead, MetaKind, MetaSigner, PalletRules, ParcelChange,
};
use crate::util::audit_utils::VerifiedOffice;
use crate::util::office_utils::{self, OfficeState};
use crate::util::object_utils;

/// The name of the manifest meta pallet. Lives in the meta namespace, so it is reached
/// as `@manifest` and reserves no user pallet name (DESIGN.html §3.3).
pub const MANIFEST_PALLET_NAME: &str = "manifest";

const FIELD_BODY: &str = "body";
const FIELD_MODEL: &str = "model";
const FIELD_TOOL: &str = "tool";
const FIELD_SESSION: &str = "session";
const FIELD_TRANSCRIPT: &str = "transcript";
const FIELD_RECORDED_AT: &str = "recorded_at";
const FIELD_SOURCE: &str = "source";
const FIELD_TRAIL_HEAD: &str = "trail_head";
const FIELD_CHECKPOINTS: &str = "checkpoints";

/// What a manifest entry asserts about its subject parcel.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ManifestKind {
    /// A free-form note.
    Note,
    /// A sign-off on the subject parcel.
    Approval,
    /// Machine-authorship provenance: how the subject parcel came to be (§7.2).
    Provenance,
    /// A delivery record: the subject parcel squashed a draft's checkpoint trail (§7.3).
    Delivery,
}

impl ManifestKind {
    /// The string form, which is also the kind's directory under a subject.
    pub fn as_str(self) -> &'static str {
        match self {
            ManifestKind::Note => "note",
            ManifestKind::Approval => "approval",
            ManifestKind::Provenance => "provenance",
            ManifestKind::Delivery => "delivery",
        }
    }

    /// Parse a kind from its string form.
    pub fn parse(value: &str) -> Result<ManifestKind, String> {
        match value {
            "note" => Ok(ManifestKind::Note),
            "approval" => Ok(ManifestKind::Approval),
            "provenance" => Ok(ManifestKind::Provenance),
            "delivery" => Ok(ManifestKind::Delivery),
            other => Err(format!(
                "\"{}\" is not a manifest entry kind (expected \"note\", \"approval\", \"provenance\" or \"delivery\").",
                other
            )),
        }
    }

    /// The one value schema this build writes and reads for the kind.
    pub fn schema(self) -> &'static str {
        match self {
            ManifestKind::Note => "manifest.note 1",
            ManifestKind::Approval => "manifest.approval 1",
            ManifestKind::Provenance => "manifest.provenance 1",
            ManifestKind::Delivery => "manifest.delivery 1",
        }
    }

    const ALL: [ManifestKind; 4] =
        [ManifestKind::Note, ManifestKind::Approval, ManifestKind::Provenance, ManifestKind::Delivery];

    fn of_schema(schema: &str) -> Option<ManifestKind> {
        ManifestKind::ALL.into_iter().find(|kind| kind.schema() == schema)
    }
}

/// A delivery record (§7.3): the subject parcel is a single clean squash of a draft
/// pallet's checkpoint trail. It *references* the kept trail (by its tip and pallet) so
/// the "what the agent tried, in what order" evidence stays discoverable from the clean
/// parcel, while the trail's checkpoints stay out of the target pallet's history.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delivery {
    /// The draft pallet the trail was delivered from.
    pub source: String,

    /// The trail tip — the draft head at delivery. `history` on it walks the full trail.
    pub trail_head: String,

    /// How many checkpoint parcels the delivery squashed.
    pub checkpoints: i64,
}

/// Machine-authorship provenance (§7.2): how a parcel came to be. Present only on a
/// `Provenance` entry. Because the entry is signed, this is *evidence* — combined with the
/// agent-class identity of the signer (§7.1), it answers "which model produced this
/// change, under whose supervision" forge-proof and offline.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Provenance {
    /// The model that produced the change — the compliance-critical field.
    pub model: String,

    /// The tool or agent that ran the model (e.g. `claude-code`).
    pub tool: Option<String>,

    /// The session / conversation id the change came from.
    pub session: Option<String>,

    /// A hash or fingerprint of the prompt or transcript: a commitment the signer makes
    /// at authorship time, so the transcript can be verified against it later.
    pub transcript: Option<String>,
}

/// One post-metadata statement about a subject parcel. Deliberately carries no author
/// field: the author is the signer stamped on the entry's cell, which is forge-proof, unlike a
/// self-declared string.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ManifestEntry {
    /// The parcel this entry is about.
    pub subject: String,

    /// What the entry asserts.
    pub kind: ManifestKind,

    /// When a provenance entry was recorded — display metadata only, never a security input.
    /// Only provenance carries a time, so the other kinds' identical entries collapse.
    pub recorded_at: Option<i64>,

    /// The entry's message (may be empty, e.g. a bare approval).
    pub body: String,

    /// The provenance details, present exactly when `kind` is `Provenance`.
    pub provenance: Option<Provenance>,

    /// The delivery details, present exactly when `kind` is `Delivery`.
    pub delivery: Option<Delivery>,
}

/// An entry together with its forge-proof authorship.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedEntry {
    pub entry: ManifestEntry,

    /// The operator whose key the cell is stamped with, or the key id when the office does not
    /// know that key.
    pub author: String,

    /// The key id the cell is stamped with.
    pub signer: String,
}

/// A manifest cell this build cannot read: a newer kind or schema, or a malformed cell. It is
/// listed, never dropped and never allowed to fail a read of the others.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableEntry {
    /// The subject, when the path names one.
    pub subject: Option<String>,

    /// The cell path, relative to `cells/`.
    pub path: String,

    /// Why it cannot be read.
    pub reason: String,
}

/// The manifest at one head.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ManifestRead {
    /// Every readable entry, in cell path order.
    pub entries: Vec<AttributedEntry>,

    /// Every cell this build cannot read.
    pub unreadable: Vec<UnreadableEntry>,
}

/// The blake3 of an operator identifier, as `approval/<op>` names it.
pub fn operator_leaf(operator: &str) -> String {
    blake3::hash(operator.as_bytes()).to_hex().to_string()
}

/// The id of a multi-valued entry: the blake3 of every field of the entry, each length-prefixed
/// so no two field lists share an encoding. The writer is not part of it — the id sits under the
/// writer's own directory.
pub fn entry_id(entry: &ManifestEntry) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };

    let optional = |text: &Option<String>| match text {
        Some(text) => format!("+{}", text),
        None => "-".to_string(),
    };

    field(entry.subject.as_bytes());
    field(entry.kind.as_str().as_bytes());
    field(entry.body.as_bytes());
    field(entry.recorded_at.map(|at| at.to_string()).unwrap_or_default().as_bytes());

    if let Some(provenance) = &entry.provenance {
        field(provenance.model.as_bytes());
        field(optional(&provenance.tool).as_bytes());
        field(optional(&provenance.session).as_bytes());
        field(optional(&provenance.transcript).as_bytes());
    }

    if let Some(delivery) = &entry.delivery {
        field(delivery.source.as_bytes());
        field(delivery.trail_head.as_bytes());
        field(delivery.checkpoints.to_string().as_bytes());
    }

    hasher.finalize().to_hex().to_string()
}

/// The cell path of an entry by `operator`, relative to `cells/`.
pub fn cell_path(operator: &str, entry: &ManifestEntry) -> String {
    let (subject, kind) = (&entry.subject, entry.kind.as_str());
    let owned = format!("{}/{}/{}/{}", &subject[..2], subject, kind, operator_leaf(operator));

    match entry.kind {
        ManifestKind::Approval => owned,
        _ => format!("{}/{}", owned, entry_id(entry)),
    }
}

/// The parts of a manifest cell path. The kind is kept as spelled, so a newer kind still parses.
struct CellPath<'a> {
    subject: &'a str,
    kind: &'a str,

    /// The writer's `<op>` component.
    owner: &'a str,

    /// The entry id, for a multi-valued entry.
    id: Option<&'a str>,
}

/// Split a cell path, refusing any path that is not `<s[0..2]>/<s>/<kind>/<op>[/<id>]` for a
/// parcel hash `s` and 64-hex `op` and `id`.
fn parse_cell_path(path: &str) -> Result<CellPath<'_>, String> {
    let parts: Vec<&str> = path.split('/').collect();

    let (fan, subject, kind, owner, id) = match parts.as_slice() {
        [fan, subject, kind, owner] => (*fan, *subject, *kind, *owner, None),
        [fan, subject, kind, owner, id] => (*fan, *subject, *kind, *owner, Some(*id)),
        _ => return Err(format!("{} is not a <subject>/<kind>/<writer>[/<id>] cell path", path)),
    };

    if !object_utils::is_object_hash(subject) || fan != &subject[..2] {
        return Err(format!("{} does not sit under the directory of a parcel hash", path));
    }

    meta_tree::validate_component(kind)?;

    if !object_utils::is_object_hash(owner) || !id.is_none_or(object_utils::is_object_hash) {
        return Err(format!("{} does not name its writer and id by 64-hex hashes", path));
    }

    Ok(CellPath { subject, kind, owner, id })
}

/// The value table of an entry.
fn entry_value(entry: &ManifestEntry) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_BODY, value(entry.body.as_str()));

    if let Some(provenance) = &entry.provenance {
        table.insert(FIELD_MODEL, value(provenance.model.as_str()));

        for (field, text) in [
            (FIELD_TOOL, &provenance.tool),
            (FIELD_SESSION, &provenance.session),
            (FIELD_TRANSCRIPT, &provenance.transcript),
        ] {
            if let Some(text) = text {
                table.insert(field, value(text.as_str()));
            }
        }
    }

    if let Some(recorded_at) = entry.recorded_at {
        table.insert(FIELD_RECORDED_AT, value(recorded_at));
    }

    if let Some(delivery) = &entry.delivery {
        table.insert(FIELD_SOURCE, value(delivery.source.as_str()));
        table.insert(FIELD_TRAIL_HEAD, value(delivery.trail_head.as_str()));
        table.insert(FIELD_CHECKPOINTS, value(delivery.checkpoints));
    }

    table
}

/// Parse an entry of a known kind from its value table, strictly: exactly the kind's fields.
fn parse_value(subject: &str, kind: ManifestKind, table: &Table) -> Result<ManifestEntry, String> {
    let mut entry = ManifestEntry {
        subject: subject.to_string(),
        kind,
        recorded_at: None,
        body: meta_tree::value_string(table, FIELD_BODY)?,
        provenance: None,
        delivery: None,
    };

    match kind {
        ManifestKind::Note | ManifestKind::Approval => meta_tree::value_fields_only(table, &[FIELD_BODY])?,
        ManifestKind::Provenance => {
            meta_tree::value_fields_only(table, &[
                FIELD_BODY, FIELD_MODEL, FIELD_TOOL, FIELD_SESSION, FIELD_TRANSCRIPT, FIELD_RECORDED_AT,
            ])?;

            entry.recorded_at = Some(meta_tree::value_integer(table, FIELD_RECORDED_AT)?);
            entry.provenance = Some(Provenance {
                model: meta_tree::value_string(table, FIELD_MODEL)?,
                tool: meta_tree::value_optional_string(table, FIELD_TOOL)?,
                session: meta_tree::value_optional_string(table, FIELD_SESSION)?,
                transcript: meta_tree::value_optional_string(table, FIELD_TRANSCRIPT)?,
            });
        }
        ManifestKind::Delivery => {
            meta_tree::value_fields_only(table, &[FIELD_BODY, FIELD_SOURCE, FIELD_TRAIL_HEAD, FIELD_CHECKPOINTS])?;

            entry.delivery = Some(Delivery {
                source: meta_tree::value_string(table, FIELD_SOURCE)?,
                trail_head: meta_tree::value_string(table, FIELD_TRAIL_HEAD)?,
                checkpoints: meta_tree::value_integer(table, FIELD_CHECKPOINTS)?,
            });
        }
    }

    Ok(entry)
}

/// Check an entry before it is written: a full parcel hash for a subject, and the details
/// exactly its kind carries.
fn validate_entry(entry: &ManifestEntry) -> Result<(), String> {
    if !object_utils::is_object_hash(&entry.subject) {
        return Err(format!("\"{}\" is not a full parcel hash.", entry.subject));
    }

    let shaped = match entry.kind {
        ManifestKind::Note | ManifestKind::Approval =>
            entry.provenance.is_none() && entry.delivery.is_none() && entry.recorded_at.is_none(),
        ManifestKind::Provenance =>
            entry.provenance.is_some() && entry.delivery.is_none() && entry.recorded_at.is_some(),
        ManifestKind::Delivery =>
            entry.delivery.is_some() && entry.provenance.is_none() && entry.recorded_at.is_none(),
    };

    if !shaped {
        return Err(format!("A {} entry does not carry the details its kind needs.", entry.kind.as_str()));
    }

    Ok(())
}

/// The operator a stamped key belongs to.
fn operator_of<'o>(office: &'o OfficeState, key_id: &str) -> Option<&'o str> {
    office.find_key(key_id).map(|key| key.operator.as_str())
}

/// The pallet rules of `@manifest`: the cell layout, the value shapes, and ownership — a parcel
/// may add, change or remove cells only under its own signer's `<op>` directory, whatever schema
/// they carry, and a known entry only at the id its content derives.
pub struct ManifestRules;

impl PalletRules for ManifestRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Manifest
    }

    fn knows_schema(&self, schema: &str) -> bool {
        ManifestKind::of_schema(schema).is_some()
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        let owner = operator_leaf(&change.signer.operator);

        for cell in change.changes {
            let path = parse_cell_path(&cell.path)?;

            if path.owner != owner {
                return Err(format!(
                    "the cell at {} sits under another operator's directory, so only they may write or remove it",
                    cell.path
                ));
            }

            let Some(after) = &cell.after else { continue };
            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;
            let kind = ManifestKind::parse(path.kind).ok();

            // An approval is the writer's directory itself, whatever schema it carries.
            if kind == Some(ManifestKind::Approval) && path.id.is_some() {
                return Err(format!("the approval at {} is not at its signer's own path", cell.path));
            }

            let Some(schema_kind) = ManifestKind::of_schema(&envelope.schema) else { continue };

            if kind != Some(schema_kind) {
                return Err(format!(
                    "the cell at {} holds a {} entry under the {} directory",
                    cell.path, schema_kind.as_str(), path.kind
                ));
            }

            let entry = parse_value(path.subject, schema_kind, &envelope.value)
                .map_err(|why| format!("the value at {} {}", cell.path, why))?;

            let expected = match schema_kind {
                ManifestKind::Approval => None,
                _ => Some(entry_id(&entry)),
            };

            if path.id != expected.as_deref() {
                return Err(format!("the entry at {} is not at the id its content derives", cell.path));
            }
        }

        Ok(())
    }

    /// `<subject>` or any path beneath it, for a full parcel hash.
    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        let subject = spelled.split('/').next().unwrap_or_default();

        if !object_utils::is_object_hash(subject) {
            return Err(format!(
                "\"{}\" does not name a subject by its full parcel hash (64 lowercase hex characters).",
                subject
            ));
        }

        meta_tree::validate_cell_path(spelled)?;

        Ok(format!("{}/{}", &subject[..2], spelled))
    }

    fn display_path(&self, path: &str) -> String {
        match path.split_once('/') {
            Some((_fan, rest)) => rest.to_string(),
            None => path.to_string(),
        }
    }

}

/// Record an entry, as one signed cell edit on the local `@manifest` head. The caller holds
/// the warehouse lock and passes the verified office.
///
/// # Returns
/// * `Ok(Some(hash))` - The new head.
/// * `Ok(None)`       - The signer had already recorded exactly this entry.
pub fn record_entry(entry: &ManifestEntry,
                    office: &VerifiedOffice,
                    actor: &Operator,
                    description: &str,
                    signer: &MetaSigner) -> Result<Option<String>, String> {
    validate_entry(entry)?;

    let operator = office.find_key(&signer.key_id)
        .ok_or(format!("The signing key {} is not in the office.", signer.key_id))?
        .operator
        .clone();

    let path = cell_path(&operator, entry);
    let edit = meta_tree::set_cell(&path, &signer.key_id, entry.kind.schema(), &entry_value(entry))?;

    meta_tree::stack_meta_edits(MANIFEST_PALLET_NAME, &ManifestRules, office, &[edit], actor, description, signer)
}

/// Read the manifest at the local head: every entry, attributed through the office (read
/// best-effort, so an entry whose key the local office does not know is attributed by key id),
/// and every cell this build cannot read. An unborn manifest reads as empty.
pub fn read_manifest() -> Result<ManifestRead, String> {
    let Some(state) = meta_tree::read_meta_head(MANIFEST_PALLET_NAME, &ManifestRules)? else {
        return Ok(ManifestRead::default());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut read = ManifestRead::default();

    for (path, cell) in state.cells {
        let parsed = parse_cell_path(&path);
        let subject = parsed.as_ref().ok().map(|parsed| parsed.subject.to_string());

        let mut unreadable = |reason: String| read.unreadable.push(UnreadableEntry {
            subject: subject.clone(), path: path.clone(), reason,
        });

        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(why) => {
                unreadable(why);
                continue;
            }
        };

        let envelope = match cell {
            CellRead::Readable(envelope) => envelope,
            CellRead::Unknown { schema, .. } => {
                unreadable(format!("written with the schema \"{}\", which this forklift does not know", schema));
                continue;
            }
            CellRead::Malformed(why) => {
                unreadable(why);
                continue;
            }
        };

        // A readable schema always names a known kind (see `knows_schema`).
        let Some(kind) = ManifestKind::of_schema(&envelope.schema) else { continue };

        if parsed.kind != kind.as_str() {
            unreadable(format!("a {} entry under the {} directory", kind.as_str(), parsed.kind));
            continue;
        }

        if (kind == ManifestKind::Approval) != parsed.id.is_none() {
            unreadable(format!("a {} entry at a path its kind does not take", kind.as_str()));
            continue;
        }

        match parse_value(parsed.subject, kind, &envelope.value) {
            Ok(entry) => read.entries.push(AttributedEntry {
                entry,
                author: operator_of(&office, &envelope.signer).unwrap_or(&envelope.signer).to_string(),
                signer: envelope.signer,
            }),
            Err(why) => unreadable(format!("the value {}", why)),
        }
    }

    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> String {
        "ab".to_string() + &"c".repeat(62)
    }

    fn entry(kind: ManifestKind, body: &str) -> ManifestEntry {
        ManifestEntry {
            subject: subject(), kind, recorded_at: None, body: body.to_string(), provenance: None, delivery: None,
        }
    }

    fn provenance(tool: Option<&str>) -> ManifestEntry {
        let mut entry = entry(ManifestKind::Provenance, "generated the module");
        entry.recorded_at = Some(100);
        entry.provenance = Some(Provenance {
            model: "claude-opus-4-8".to_string(),
            tool: tool.map(str::to_string),
            session: None,
            transcript: None,
        });

        entry
    }

    #[test]
    fn every_kind_round_trips_through_its_value() {
        let mut delivery = entry(ManifestKind::Delivery, "delivered");
        delivery.delivery = Some(Delivery { source: "draft".to_string(), trail_head: "d".repeat(64), checkpoints: 3 });

        for original in [
            entry(ManifestKind::Note, "multi\nline"),
            entry(ManifestKind::Approval, ""),
            provenance(Some("claude-code")),
            provenance(None),
            delivery,
        ] {
            validate_entry(&original).unwrap();
            assert_eq!(parse_value(&subject(), original.kind, &entry_value(&original)).unwrap(), original);
        }
    }

    #[test]
    fn a_value_is_parsed_strictly() {
        let mut extra = entry_value(&entry(ManifestKind::Note, "x"));
        extra.insert(FIELD_MODEL, value("m"));
        let error = parse_value(&subject(), ManifestKind::Note, &extra).unwrap_err();
        assert!(error.contains("unexpected field \"model\""), "{}", error);

        // A present field of the wrong type is an error, never an absence: a query by tool
        // would otherwise miss an entry that carries one.
        for field in [FIELD_TOOL, FIELD_SESSION, FIELD_TRANSCRIPT] {
            let mut wrong = entry_value(&provenance(None));
            wrong.insert(field, value(7));
            let error = parse_value(&subject(), ManifestKind::Provenance, &wrong).unwrap_err();
            assert!(error.contains(field) && error.contains("not a string"), "{}", error);
        }

        let mut timeless = entry_value(&provenance(None));
        timeless.remove(FIELD_RECORDED_AT);
        assert!(parse_value(&subject(), ManifestKind::Provenance, &timeless).is_err());
    }

    #[test]
    fn ids_collapse_one_writers_duplicates_and_separate_two_writers() {
        let note = entry(ManifestKind::Note, "ok");

        assert_eq!(cell_path("alice", &note), cell_path("alice", &note.clone()));
        assert_ne!(cell_path("alice", &note), cell_path("bob", &note), "each writer has a directory");
        assert_ne!(entry_id(&note), entry_id(&entry(ManifestKind::Note, "nope")));
        assert_ne!(entry_id(&note), entry_id(&entry(ManifestKind::Approval, "ok")));
        assert_ne!(entry_id(&provenance(Some("x"))), entry_id(&provenance(None)));

        // An approval is one per operator, whatever it says.
        let (approve_a, approve_b) = (entry(ManifestKind::Approval, "a"), entry(ManifestKind::Approval, "b"));
        assert_eq!(cell_path("alice", &approve_a), cell_path("alice", &approve_b));
        assert_ne!(cell_path("alice", &approve_a), cell_path("bob", &approve_a));
    }

    #[test]
    fn a_cell_path_round_trips_and_nothing_else_parses() {
        let note = entry(ManifestKind::Note, "x");
        let parsed_note = cell_path("alice", &note);
        let parsed = parse_cell_path(&parsed_note).unwrap();
        assert_eq!((parsed.subject, parsed.kind), (subject().as_str(), "note"));
        assert_eq!((parsed.owner, parsed.id), (operator_leaf("alice").as_str(), Some(entry_id(&note).as_str())));

        let approval = cell_path("alice", &entry(ManifestKind::Approval, ""));
        assert_eq!(parse_cell_path(&approval).unwrap().id, None);

        let hex = "e".repeat(64);
        for wrong in [
            format!("ab/{}/note", subject()),
            format!("cd/{}/note/{}", subject(), hex),
            format!("ab/{}/note/short", subject()),
            format!("ab/{}/note/{}/short", subject(), hex),
            format!("ab/{}/note/{}/{}/deeper", subject(), hex, hex),
        ] {
            assert!(parse_cell_path(&wrong).is_err(), "{} must not parse", wrong);
        }
    }

    #[test]
    fn an_entry_without_its_kinds_details_is_refused_before_writing() {
        let mut bare = entry(ManifestKind::Provenance, "x");
        assert!(validate_entry(&bare).is_err());
        bare.kind = ManifestKind::Note;
        bare.recorded_at = Some(1);
        assert!(validate_entry(&bare).is_err(), "only provenance carries a time");

        let mut short = entry(ManifestKind::Note, "x");
        short.subject = "abc".to_string();
        assert!(validate_entry(&short).is_err());
    }
}
