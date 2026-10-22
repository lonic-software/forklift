//! Signed tags / releases (§9.4d): a named, signed pointer at a parcel.
//!
//! A tag marks a parcel — a release, a milestone — with a name. The Forklift twist is the
//! same one the manifest uses: the tag is a signed cell on a meta pallet (`@tags`, on the
//! meta-tree substrate, `meta_tree`), so who tagged it is the cell's signer stamp, not a
//! self-declared field, and it is **verifiable offline against the office chain**.
//!
//! A tag sits at `cells/<name>` and holds a `tags.tag 1` value: the subject, the message and
//! when it was tagged. No parcel may change or remove an existing tag, wherever the content check
//! runs, so a tag name is immutable everywhere. And a parcel that adds a tag must be signed by an
//! Admin of the office as it stands when the parcel is admitted (the release convention): when a
//! client writes or replays it, and when a remote moves its head. A client adopting history the
//! remote admitted does not re-check the role, so a later demotion never wedges it; that leaves
//! the role, like every office privilege, as the remote enforces it.

use toml_edit::{value, Table};
use crate::model::operator::Operator;
use crate::util::meta_tree::{
    self, Admission, CellEnvelope, CellRead, MetaKind, MetaSigner, PalletRules, ParcelChange,
};
use crate::util::audit_utils::VerifiedOffice;
use crate::util::office_utils::{self, OfficeState, Role};
use crate::util::object_utils;

/// The name of the tags meta pallet. Lives in the meta namespace, so it is reached as
/// `@tags` and reserves no user pallet name (DESIGN.html §3.3).
pub const TAGS_PALLET_NAME: &str = "tags";

/// The one value schema this build writes and reads.
pub const TAG_SCHEMA: &str = "tags.tag 1";

/// The longest tag name, in bytes.
pub const MAX_TAG_NAME_BYTES: usize = 128;

const FIELD_SUBJECT: &str = "subject";
const FIELD_MESSAGE: &str = "message";
const FIELD_TAGGED_AT: &str = "tagged_at";

/// One signed tag: a named pointer at a subject parcel. Carries no tagger field — the
/// tagger is the signer stamped on its cell, which is forge-proof, unlike a self-declared
/// string.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tag {
    /// The tag name (a release label, e.g. `v1.2.0`). Immutable once created.
    pub name: String,

    /// The parcel the tag points at.
    pub subject: String,

    /// The tag message (may be empty).
    pub message: String,

    /// When the tag was created. Display metadata only — never a security input.
    pub tagged_at: i64,
}

/// A tag together with its forge-proof authorship.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedTag {
    pub tag: Tag,

    /// The operator whose key the cell is stamped with, or the key id when the office does not
    /// know that key.
    pub tagger: String,

    /// The key id the cell is stamped with.
    pub signer: String,
}

/// A tag cell this build cannot read: a newer schema, or a malformed cell. It is listed, never
/// dropped and never allowed to fail a read of the others.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableTag {
    /// The tag name — the cell's path.
    pub name: String,

    /// Why it cannot be read.
    pub reason: String,
}

/// Every tag at one head.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TagsRead {
    /// Every readable tag, by name.
    pub tags: Vec<AttributedTag>,

    /// Every tag cell this build cannot read.
    pub unreadable: Vec<UnreadableTag>,
}

/// Validate a tag name. Tags are flat release labels: one metadata path component (ASCII
/// letters, digits, `.`, `_` and `-`, never starting with `.` or `-`) of at most
/// [`MAX_TAG_NAME_BYTES`] bytes.
pub fn validate_tag_name(name: &str) -> Result<(), String> {
    meta_tree::validate_component(name)
        .map_err(|why| format!("\"{}\" is not a valid tag name: {}", name, why))?;

    if name.len() > MAX_TAG_NAME_BYTES {
        return Err(format!(
            "The tag name \"{}…\" is {} bytes long; a tag name may be at most {} bytes.",
            name.chars().take(32).collect::<String>(), name.len(), MAX_TAG_NAME_BYTES
        ));
    }

    Ok(())
}

/// A `tags.tag 1` value table.
fn tag_value(tag: &Tag) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_SUBJECT, value(tag.subject.as_str()));
    table.insert(FIELD_MESSAGE, value(tag.message.as_str()));
    table.insert(FIELD_TAGGED_AT, value(tag.tagged_at));

    table
}

/// Parse a `tags.tag 1` value table, strictly: exactly its three fields, and a subject that is
/// a full parcel hash.
fn parse_value(name: &str, table: &Table) -> Result<Tag, String> {
    meta_tree::value_fields_only(table, &[FIELD_SUBJECT, FIELD_MESSAGE, FIELD_TAGGED_AT])?;

    let subject = meta_tree::value_string(table, FIELD_SUBJECT)?;

    if !object_utils::is_object_hash(&subject) {
        return Err("has a subject that is not a full parcel hash".to_string());
    }

    Ok(Tag {
        name: name.to_string(),
        subject,
        message: meta_tree::value_string(table, FIELD_MESSAGE)?,
        tagged_at: meta_tree::value_integer(table, FIELD_TAGGED_AT)?,
    })
}

/// The pallet rules of `@tags`: one cell per valid name, a strict value, an Admin to add one
/// (checked when a parcel is admitted, not when admitted history is audited), and immutability.
pub struct TagsRules;

impl PalletRules for TagsRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Tags
    }

    fn knows_schema(&self, schema: &str) -> bool {
        schema == TAG_SCHEMA
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        for cell in change.changes {
            validate_tag_name(&cell.path)?;

            if cell.before.is_some() {
                return Err(format!("the tag {} already exists, and a tag is never changed or removed", cell.path));
            }

            let Some(after) = &cell.after else { continue };

            if change.admission == Admission::Admit {
                let operator = &change.signer.operator;
                let is_admin = change.office.find_user(operator)
                    .is_some_and(|user| matches!(user.role, Role::Admin));

                if !is_admin {
                    return Err(format!("the tag {} is signed by {}, who is not an admin", cell.path, operator));
                }
            }

            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;

            if envelope.schema == TAG_SCHEMA {
                parse_value(&cell.path, &envelope.value).map_err(|why| format!("the tag {} {}", cell.path, why))?;
            }
        }

        Ok(())
    }

    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        validate_tag_name(spelled)?;

        Ok(spelled.to_string())
    }

    fn display_value(&self, envelope: &CellEnvelope) -> String {
        match meta_tree::value_string(&envelope.value, FIELD_SUBJECT) {
            Ok(subject) => format!("points at {}", subject),
            Err(why) => format!("(unreadable: the value {})", why),
        }
    }
}

/// Create a tag, as one signed cell on the local `@tags` head. The caller holds the warehouse
/// lock and passes the verified office; the content check refuses a signer who is not an
/// admin, and a name that already exists.
pub fn record_tag(tag: &Tag,
                  office: &VerifiedOffice,
                  actor: &Operator,
                  signer: &MetaSigner) -> Result<String, String> {
    validate_tag_name(&tag.name)?;

    if !object_utils::is_object_hash(&tag.subject) {
        return Err(format!("\"{}\" is not a full parcel hash.", tag.subject));
    }

    let edit = meta_tree::set_cell(&tag.name, &signer.key_id, TAG_SCHEMA, &tag_value(tag))?;
    let description = format!("tag {} {}", tag.name, tag.subject);

    meta_tree::stack_meta_edits(TAGS_PALLET_NAME, &TagsRules, office, &[edit], actor, &description, signer)?
        .ok_or(format!("Tag \"{}\" already exists.", tag.name))
}

/// Read every tag at the local head, attributed through the office (read best-effort, so a
/// tag whose key the local office does not know is attributed by key id), with every cell this
/// build cannot read. An unborn pallet reads as empty.
pub fn read_tags() -> Result<TagsRead, String> {
    let Some(state) = meta_tree::read_meta_head(TAGS_PALLET_NAME, &TagsRules)? else {
        return Ok(TagsRead::default());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut read = TagsRead::default();

    for (name, cell) in state.cells {
        let parsed = match cell {
            CellRead::Readable(envelope) => parse_value(&name, &envelope.value)
                .map(|tag| (tag, envelope.signer))
                .map_err(|why| format!("the value {}", why)),
            CellRead::Unknown { schema, .. } =>
                Err(format!("written with the schema \"{}\", which this forklift does not know", schema)),
            CellRead::Malformed(why) => Err(why),
        };

        match parsed {
            Ok((tag, signer)) => read.tags.push(AttributedTag {
                tag,
                tagger: office.find_key(&signer).map(|key| key.operator.clone()).unwrap_or(signer.clone()),
                signer,
            }),
            Err(reason) => read.unreadable.push(UnreadableTag { name, reason }),
        }
    }

    Ok(read)
}

/// The tag of a name at the local head, or `None` when there is none. A tag cell this build
/// cannot read is an error naming why.
pub fn find_tag(name: &str) -> Result<Option<AttributedTag>, String> {
    validate_tag_name(name)?;

    let read = read_tags()?;

    if let Some(unreadable) = read.unreadable.iter().find(|tag| tag.name == name) {
        return Err(format!("Tag \"{}\" cannot be read: {}.", name, unreadable.reason));
    }

    Ok(read.tags.into_iter().find(|tag| tag.tag.name == name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag() -> Tag {
        Tag { name: "v1.0".to_string(), subject: "a".repeat(64), message: "first".to_string(), tagged_at: 7 }
    }

    #[test]
    fn a_tag_round_trips_through_its_value() {
        assert_eq!(parse_value("v1.0", &tag_value(&tag())).unwrap(), tag());
    }

    #[test]
    fn a_value_is_parsed_strictly() {
        let mut extra = tag_value(&tag());
        extra.insert("tagger", value("mallory"));
        assert!(parse_value("v1.0", &extra).unwrap_err().contains("unexpected field \"tagger\""));

        let mut short = tag();
        short.subject = "abc".to_string();
        assert!(parse_value("v1.0", &tag_value(&short)).unwrap_err().contains("full parcel hash"));

        let mut untimed = tag_value(&tag());
        untimed.insert(FIELD_TAGGED_AT, value("yesterday"));
        assert!(parse_value("v1.0", &untimed).unwrap_err().contains("not an integer"));
    }

    #[test]
    fn tag_names_are_one_bounded_component() {
        for good in ["v1.2.0", "release_2", "rc-1", "a"] {
            assert!(validate_tag_name(good).is_ok(), "{}", good);
        }

        for bad in ["", "-v1", ".hidden", "a/b", "v 1", "ü", &"t".repeat(MAX_TAG_NAME_BYTES + 1)] {
            assert!(validate_tag_name(bad).is_err(), "{:?} must be refused", bad);
        }

        assert!(validate_tag_name(&"t".repeat(MAX_TAG_NAME_BYTES)).is_ok());
    }
}
