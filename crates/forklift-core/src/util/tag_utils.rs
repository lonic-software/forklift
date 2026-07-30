//! Signed tags / releases (§9.4d): a named, signed pointer at a parcel.
//!
//! A tag marks a parcel — a release, a milestone — with a name. The Forklift twist is the
//! same one the manifest uses: the tag is a signed record on a meta pallet, so who tagged
//! it is the parcel's signature, not a self-declared field, and it is **verifiable offline
//! against the office chain**. The on-brand convention (§9.4d) is a release tag signed by
//! an *admin* key — an authoritative act — which the `tag` command enforces at creation.
//!
//! Tags live on their own meta pallet (`@tags`, at `.forklift/meta/tags`), reached as
//! `@tags` and reserving no user pallet name — the exact pattern the office and `@manifest`
//! established. The pallet is an append-only DAG of single-record parcels; reading unions
//! every reachable record, so two diverged tag pallets merge with a plain join. A tag name
//! is immutable: `tag create` refuses a name already reachable from the head.

use std::collections::HashSet;
use toml_edit::{value, DocumentMut};
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::blob::Blob;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::model::tree_item::TreeItem;
use chrono::Utc;
use crate::util::{object_utils, office_utils, pallet_utils, sign_utils};

/// The name of the tags meta pallet. Lives in the meta namespace, so it is reached as
/// `@tags` and reserves no user pallet name (DESIGN.html §3.3).
pub const TAGS_PALLET_NAME: &str = "tags";

/// The tree namespace of tag records: `.forklift/tracked/tags/…`. Shares the
/// `.forklift/tracked` root with the office and the manifest, inheriting the same
/// collision-proofing and materialization guard.
const TREE_NAME_FORKLIFT: &str = ".forklift";
const TREE_NAME_TRACKED: &str = "tracked";
const TREE_NAME_TAGS: &str = "tags";

/// The filename suffix of a stored tag record blob.
const RECORD_SUFFIX: &str = ".toml";

/// One signed tag: a named pointer at a subject parcel. Carries no tagger field — the
/// tagger is the signer of the parcel that introduces it (resolved by [`read_tags`]), which
/// is forge-proof, unlike a self-declared string.
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

/// A tag together with its forge-proof authorship: the operator whose key signed the parcel
/// that introduced it, and that parcel's hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedTag {
    pub tag: Tag,

    /// The operator id resolved from the introducing parcel's signature (the signing key's
    /// owner in the office). Falls back to the key id, then `"unsigned"`.
    pub tagger: String,

    /// The parcel that introduced the tag.
    pub parcel: String,
}

/// Validate a tag name. Tags are flat release labels: non-empty, ASCII letters, digits,
/// `.`, `_` and `-`, and never starting with `-` (indistinguishable from a flag). Unlike
/// pallet names, a tag name has no `/` components — it is a single label and a single file.
///
/// # Arguments
/// * `name` - The tag name to validate.
///
/// # Returns
/// * `Ok(())`      - If the name is valid.
/// * `Err(String)` - If the name is not valid.
pub fn validate_tag_name(name: &str) -> Result<(), String> {
    let error = |reason: &str| Err(format!("\"{}\" is not a valid tag name: {}", name, reason));

    if name.is_empty() {
        return error("it is empty");
    }

    if name.starts_with('-') {
        return error("it must not start with \"-\"");
    }

    let is_valid_char = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-';

    if !name.chars().all(is_valid_char) {
        return error("only ASCII letters, digits, \".\", \"_\" and \"-\" are allowed (no \"/\")");
    }

    Ok(())
}

/// Read every tag reachable from the tags pallet head, each attributed to the operator who
/// signed its parcel. Returned sorted by name, de-duplicated per (tagger, tag).
///
/// # Returns
/// * `Ok(Vec<AttributedTag>)` - The attributed tags (empty when unborn).
/// * `Err(String)`            - If an object could not be read.
pub fn read_tags() -> Result<Vec<AttributedTag>, String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME)? else {
        return Ok(Vec::new());
    };

    // The office maps a signing key to its operator; best-effort (a fresh clone that has
    // not fetched the office yet still lists tags, attributed by key id).
    let office = office_utils::read_office_state()
        .unwrap_or(office_utils::OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut collected: Vec<AttributedTag> = Vec::new();
    let mut deduped: HashSet<(String, String)> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![head];

    while let Some(hash) = queue.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        queue.extend(parcel.parents.clone());

        let Some(tags_tree) = resolve_subtree(&parcel.tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_TAGS])? else {
            continue; // A merge (join) parcel: no record.
        };

        let tagger = resolve_author(&hash, &office);

        for (_, file) in tags_tree.get_files() {
            let tag = parse_tag(&load_record(&file.hash)?)?;

            if deduped.insert((tagger.clone(), tag.name.clone())) {
                collected.push(AttributedTag { tag, tagger: tagger.clone(), parcel: hash.clone() });
            }
        }
    }

    collected.sort_by(|a, b| a.tag.name.cmp(&b.tag.name).then(a.parcel.cmp(&b.parcel)));

    Ok(collected)
}

/// Find a tag by name (the first reachable record with that name), if any.
///
/// # Arguments
/// * `name` - The tag name.
///
/// # Returns
/// * `Ok(Some(AttributedTag))` - The tag.
/// * `Ok(None)`                - If no tag by that name exists.
/// * `Err(String)`             - If an object could not be read.
pub fn find_tag(name: &str) -> Result<Option<AttributedTag>, String> {
    Ok(read_tags()?.into_iter().find(|attributed| attributed.tag.name == name))
}

/// The operator who authored a tag parcel: the owner of the key that signed it.
fn resolve_author(parcel_hash: &str, office: &office_utils::OfficeState) -> String {
    match sign_utils::load_parcel_signature(parcel_hash) {
        Ok(Some(signature)) => office.find_key(&signature.key_id)
            .map(|key| key.operator.clone())
            .unwrap_or(signature.key_id),
        _ => "unsigned".to_string(),
    }
}

/// Record a new tag: stack a single-record parcel signed by the tagger, on top of the
/// current tags head.
///
/// # Arguments
/// * `tag`            - The tag to record.
/// * `actor`          - The operator recording it.
/// * `signing_key_id` - The key to sign with (its private half must be local).
///
/// # Returns
/// * `Ok(String)`  - The hash of the new tags parcel.
/// * `Err(String)` - If an object could not be stored, or the parcel signed.
pub fn record_tag(tag: &Tag, actor: &Operator, signing_key_id: &str) -> Result<String, String> {
    let parents: Vec<String> = pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME)?
        .into_iter()
        .collect();

    let file_name = format!("{}{}", tag.name, RECORD_SUFFIX);
    let blob_hash = store_record(&tag_to_toml(tag))?;

    let mut tags_tree = TreeItem::new(TREE_NAME_TAGS.to_string(), String::new(), DirEntryType::Tree);
    tags_tree.add_child(TreeItem::new(file_name, blob_hash, DirEntryType::Normal));

    let description = format!("Tagged {} as \"{}\".", &tag.subject[..tag.subject.len().min(12)], tag.name);

    stack_tag_parcel(tags_tree, parents, actor, description, signing_key_id)
}

/// Build, sign and store a tag parcel with the given tag subtree and parents, and advance
/// the tags pallet head.
fn stack_tag_parcel(mut tags_tree: TreeItem,
                    parents: Vec<String>,
                    actor: &Operator,
                    description: String,
                    signing_key_id: &str) -> Result<String, String> {
    // Nest the record under `.forklift/tracked/tags/`, storing each subtree's object.
    store_subtree(&mut tags_tree)?;

    let mut tracked_tree = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
    tracked_tree.add_child(tags_tree);
    store_subtree(&mut tracked_tree)?;

    let mut forklift_tree = TreeItem::new(TREE_NAME_FORKLIFT.to_string(), String::new(), DirEntryType::Tree);
    forklift_tree.add_child(tracked_tree);
    store_subtree(&mut forklift_tree)?;

    let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    root_tree.add_child(forklift_tree);

    let mut root_object = LooseObjectBuilder::build_tree(&root_tree);
    root_object.store()?;

    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash: root_object.hash.clone(),
        parents,
        actions: vec![
            ParcelAction {
                operator: actor.clone(),
                action: ParcelActionType::Author,
                description: None,
                timestamp,
            },
            ParcelAction {
                operator: actor.clone(),
                action: ParcelActionType::Stack,
                description: None,
                timestamp,
            },
        ],
        description: Some(description),
    };

    let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
    parcel_object.store()?;

    let signature = sign_utils::sign_parcel_hash(signing_key_id, &parcel_object.hash)?;
    sign_utils::store_parcel_signature(&parcel_object.hash, &signature)?;

    pallet_utils::set_meta_pallet_head(TAGS_PALLET_NAME, &parcel_object.hash)?;

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
        .map_err(|_| format!("The tag record {} is not valid UTF-8.", hash))
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

/// Serialize a tag as TOML.
fn tag_to_toml(tag: &Tag) -> String {
    let mut doc = DocumentMut::new();

    doc["name"] = value(tag.name.as_str());
    doc["subject"] = value(tag.subject.as_str());
    doc["message"] = value(tag.message.as_str());
    doc["tagged_at"] = value(tag.tagged_at);

    doc.to_string()
}

/// Parse a tag from TOML.
fn parse_tag(toml: &str) -> Result<Tag, String> {
    let doc: DocumentMut = toml.parse()
        .map_err(|e| format!("A tag record is not valid TOML: {}", e))?;

    let read_string = |field: &str| -> Result<String, String> {
        doc.get(field)
            .and_then(|item| item.as_str())
            .map(|s| s.to_string())
            .ok_or(format!("A tag record has no \"{}\" entry.", field))
    };

    Ok(Tag {
        name: read_string("name")?,
        subject: read_string("subject")?,
        message: read_string("message")?,
        tagged_at: doc.get("tagged_at")
            .and_then(|item| item.as_integer())
            .ok_or("A tag record has no \"tagged_at\" entry.".to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_through_toml() {
        let original = Tag {
            name: "v1.2.0".to_string(),
            subject: "a".repeat(64),
            message: "the second release".to_string(),
            tagged_at: 1_700_000_000,
        };

        assert_eq!(parse_tag(&tag_to_toml(&original)).unwrap(), original);
    }

    #[test]
    fn valid_tag_names_are_accepted() {
        for name in ["v1", "v1.2.0", "release_2024", "rc-1"] {
            assert!(validate_tag_name(name).is_ok(), "expected valid: {}", name);
        }
    }

    #[test]
    fn invalid_tag_names_are_rejected() {
        for name in ["", "-v1", "v1/2", "with space", "emoji📦", "a\nb"] {
            assert!(validate_tag_name(name).is_err(), "expected invalid: {}", name);
        }
    }
}
