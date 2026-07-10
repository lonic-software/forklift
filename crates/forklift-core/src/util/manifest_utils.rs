//! The manifest: post-metadata about parcels as tracked metadata.
//!
//! A *manifest entry* is a signed statement attached to a parcel after the fact — an
//! approval, a review note, and (later, §7.2) machine-authorship provenance. Entries are
//! ordinary blobs under the reserved `.forklift/tracked/…` tree namespace, carried by
//! parcels on a dedicated meta pallet (`@manifest`, at `.forklift/meta/manifest`). An
//! entry *references* its subject parcel and never mutates it (the §4.4 immutability
//! invariant); the manifest is the "GitHub layer" living inside the warehouse instead of
//! a hosting provider's database.
//!
//! **The manifest is an append-only DAG of single-entry parcels, and authorship is the
//! parcel's signature — never a stored field.** Each `manifest note/approve` stacks one
//! parcel carrying one entry, signed by its author; the author *is* whoever signed it, so
//! there is nothing to forge and the ordinary `verify_pallet_history` (which checks every
//! parcel is signed by a tracked key) is the whole verification — no per-pallet server
//! special-case. Reading collects every entry *reachable* from the head, so two diverged
//! branches merge with a plain two-parent join parcel: the union of independent records,
//! never a conflict. (Contrast the office, whose interdependent records stay linear.)

use std::collections::HashSet;
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

/// The name of the manifest meta pallet. Lives in the meta namespace, so it is reached
/// as `@manifest` and reserves no user pallet name (DESIGN.html §3.3).
pub const MANIFEST_PALLET_NAME: &str = "manifest";

/// The tree namespace of manifest entries: `.forklift/tracked/manifest/…`. Shares the
/// `.forklift/tracked` root with the office, so it inherits the same collision-proofing
/// (the `.forklift` folder is never tracked) and materialization guard.
const TREE_NAME_FORKLIFT: &str = ".forklift";
const TREE_NAME_TRACKED: &str = "tracked";
const TREE_NAME_MANIFEST: &str = "manifest";

/// The filename suffix of a stored record blob.
const RECORD_SUFFIX: &str = ".toml";

/// What a manifest entry asserts about its subject parcel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// The on-chain string form.
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
    /// at authorship time, so the transcript can be verified against it later (the full
    /// transcript-as-a-blob option rides on this without weakening it).
    pub transcript: Option<String>,
}

/// One post-metadata statement about a subject parcel. Deliberately carries no author
/// field: the author is the signer of the parcel that introduces the entry (resolved by
/// [`read_manifest`]), which is forge-proof, unlike a self-declared string.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ManifestEntry {
    /// The parcel this entry is about.
    pub subject: String,

    /// What the entry asserts.
    pub kind: ManifestKind,

    /// When the entry was recorded. Display metadata only — never a security input.
    pub recorded_at: i64,

    /// The entry's message (may be empty, e.g. a bare approval).
    pub body: String,

    /// The provenance details, present exactly when `kind` is `Provenance`.
    pub provenance: Option<Provenance>,

    /// The delivery details, present exactly when `kind` is `Delivery`.
    pub delivery: Option<Delivery>,
}

impl ManifestEntry {
    /// The entry's content-derived id — the filename it is stored under.
    pub fn id(&self) -> String {
        let provenance = self.provenance.as_ref().map(|p| format!(
            "{}\n{}\n{}\n{}",
            p.model,
            p.tool.as_deref().unwrap_or(""),
            p.session.as_deref().unwrap_or(""),
            p.transcript.as_deref().unwrap_or("")
        )).unwrap_or_default();

        let delivery = self.delivery.as_ref()
            .map(|d| format!("{}\n{}\n{}", d.source, d.trail_head, d.checkpoints))
            .unwrap_or_default();

        let material = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.subject, self.kind.as_str(), self.recorded_at, self.body, provenance, delivery
        );

        blake3::hash(material.as_bytes()).to_hex().to_string()
    }
}

/// An entry together with its forge-proof authorship: the operator whose key signed the
/// parcel that introduced it, and that parcel's hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedEntry {
    pub entry: ManifestEntry,

    /// The operator id resolved from the introducing parcel's signature (the signing
    /// key's owner in the office). Falls back to the key id, then `"unsigned"`, when the
    /// author cannot be resolved.
    pub author: String,

    /// The manifest parcel that introduced the entry.
    pub parcel: String,
}

/// Read the whole manifest: every entry reachable from the manifest pallet head, each
/// attributed to the operator who signed its parcel. Entries are returned oldest first
/// (by record time, then parcel hash) and de-duplicated per (author, entry).
///
/// # Returns
/// * `Ok(Vec<AttributedEntry>)` - The attributed entries (empty when unborn).
/// * `Err(String)`              - If an object could not be read.
pub fn read_manifest() -> Result<Vec<AttributedEntry>, String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(MANIFEST_PALLET_NAME)? else {
        return Ok(Vec::new());
    };

    // The office maps a signing key to its operator; best-effort (a fresh clone that has
    // not fetched the office yet still lists entries, attributed by key id).
    let office = office_utils::read_office_state()
        .unwrap_or(office_utils::OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut collected: Vec<AttributedEntry> = Vec::new();
    let mut deduped: HashSet<(String, String)> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: Vec<String> = vec![head];

    while let Some(hash) = queue.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        queue.extend(parcel.parents.clone());

        let Some(manifest) = resolve_subtree(&parcel.tree_hash, &[TREE_NAME_FORKLIFT, TREE_NAME_TRACKED, TREE_NAME_MANIFEST])? else {
            continue; // A merge parcel (or non-manifest parcel): no entry.
        };

        let author = resolve_author(&hash, &office);

        for (_, file) in manifest.get_files() {
            let entry = parse_entry(&load_record(&file.hash)?)?;

            if deduped.insert((author.clone(), entry.id())) {
                collected.push(AttributedEntry { entry, author: author.clone(), parcel: hash.clone() });
            }
        }
    }

    collected.sort_by(|a, b| a.entry.recorded_at.cmp(&b.entry.recorded_at).then(a.parcel.cmp(&b.parcel)));

    Ok(collected)
}

/// The operator who authored a manifest parcel: the owner of the key that signed it.
fn resolve_author(parcel_hash: &str, office: &office_utils::OfficeState) -> String {
    match sign_utils::load_parcel_signature(parcel_hash) {
        Ok(Some(signature)) => office.find_key(&signature.key_id)
            .map(|key| key.operator.clone())
            .unwrap_or(signature.key_id),
        _ => "unsigned".to_string(),
    }
}

/// Record a new manifest entry: stack a single-entry parcel signed by the author, on top
/// of the current manifest head.
///
/// # Arguments
/// * `entry`          - The entry to record.
/// * `actor`          - The operator recording it.
/// * `description`    - The parcel description (the audit line).
/// * `signing_key_id` - The key to sign with (its private half must be local).
///
/// # Returns
/// * `Ok(String)`  - The hash of the new manifest parcel.
/// * `Err(String)` - If an object could not be stored, or the parcel signed.
pub fn record_entry(entry: &ManifestEntry,
                    actor: &Operator,
                    description: String,
                    signing_key_id: &str) -> Result<String, String> {
    let parents: Vec<String> = pallet_utils::get_meta_pallet_head(MANIFEST_PALLET_NAME)?
        .into_iter()
        .collect();

    let file_name = format!("{}{}", entry.id(), RECORD_SUFFIX);
    let blob_hash = store_record(&entry_to_toml(entry))?;

    let mut manifest_tree = TreeItem::new(TREE_NAME_MANIFEST.to_string(), String::new(), DirEntryType::Tree);
    manifest_tree.add_child(TreeItem::new(file_name, blob_hash, DirEntryType::Normal));

    stack_manifest_parcel(Some(manifest_tree), parents, actor, description, signing_key_id)
}

/// Merge another manifest head into the local one: a two-parent join parcel that carries
/// no entry of its own. Because reading unions every reachable entry, the join is the
/// merge — no conflict is possible between independent records.
///
/// # Arguments
/// * `other_head`     - The other manifest head to merge in.
/// * `actor`          - The operator performing the merge.
/// * `description`    - The parcel description.
/// * `signing_key_id` - The key to sign the join parcel with.
///
/// # Returns
/// * `Ok(String)`  - The hash of the new (merge) manifest parcel.
/// * `Err(String)` - If the local manifest is unborn, or an object could not be signed.
pub fn merge_manifest(other_head: &str,
                      actor: &Operator,
                      description: String,
                      signing_key_id: &str) -> Result<String, String> {
    let local_head = pallet_utils::get_meta_pallet_head(MANIFEST_PALLET_NAME)?
        .ok_or("There is no local manifest to merge into.".to_string())?;

    let parents = vec![local_head, other_head.to_string()];

    stack_manifest_parcel(None, parents, actor, description, signing_key_id)
}

/// Build, sign and store a manifest parcel with the given (optional) manifest subtree and
/// parents, and advance the manifest pallet head. `entry_subtree` is `None` for a merge
/// (join) parcel, which carries no entry of its own.
fn stack_manifest_parcel(entry_subtree: Option<TreeItem>,
                         parents: Vec<String>,
                         actor: &Operator,
                         description: String,
                         signing_key_id: &str) -> Result<String, String> {
    let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

    // A merge parcel has an empty root tree; a record parcel nests its single entry under
    // `.forklift/tracked/manifest/`.
    if let Some(mut manifest_tree) = entry_subtree {
        store_subtree(&mut manifest_tree)?;

        let mut tracked_tree = TreeItem::new(TREE_NAME_TRACKED.to_string(), String::new(), DirEntryType::Tree);
        tracked_tree.add_child(manifest_tree);
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

    pallet_utils::set_meta_pallet_head(MANIFEST_PALLET_NAME, &parcel_object.hash)?;

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
        .map_err(|_| format!("The manifest record {} is not valid UTF-8.", hash))
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

/// Serialize a manifest entry as TOML.
fn entry_to_toml(entry: &ManifestEntry) -> String {
    let mut doc = DocumentMut::new();

    doc["subject"] = value(entry.subject.as_str());
    doc["kind"] = value(entry.kind.as_str());
    doc["recorded_at"] = value(entry.recorded_at);
    doc["body"] = value(entry.body.as_str());

    // Provenance fields (present only on a provenance entry).
    if let Some(provenance) = &entry.provenance {
        doc["model"] = value(provenance.model.as_str());

        if let Some(tool) = &provenance.tool {
            doc["tool"] = value(tool.as_str());
        }
        if let Some(session) = &provenance.session {
            doc["session"] = value(session.as_str());
        }
        if let Some(transcript) = &provenance.transcript {
            doc["transcript"] = value(transcript.as_str());
        }
    }

    // Delivery fields (present only on a delivery entry).
    if let Some(delivery) = &entry.delivery {
        doc["source"] = value(delivery.source.as_str());
        doc["trail_head"] = value(delivery.trail_head.as_str());
        doc["checkpoints"] = value(delivery.checkpoints);
    }

    doc.to_string()
}

/// Parse a manifest entry from TOML.
fn parse_entry(toml: &str) -> Result<ManifestEntry, String> {
    let doc: DocumentMut = toml.parse()
        .map_err(|e| format!("A manifest entry is not valid TOML: {}", e))?;

    let read_string = |field: &str| -> Result<String, String> {
        doc.get(field)
            .and_then(|item| item.as_str())
            .map(|s| s.to_string())
            .ok_or(format!("A manifest entry has no \"{}\" entry.", field))
    };

    let optional_string = |field: &str| -> Option<String> {
        doc.get(field).and_then(|item| item.as_str()).map(|s| s.to_string())
    };

    let kind = ManifestKind::parse(&read_string("kind")?)?;

    let provenance = match kind {
        ManifestKind::Provenance => Some(Provenance {
            model: read_string("model")?,
            tool: optional_string("tool"),
            session: optional_string("session"),
            transcript: optional_string("transcript"),
        }),
        _ => None,
    };

    let delivery = match kind {
        ManifestKind::Delivery => Some(Delivery {
            source: read_string("source")?,
            trail_head: read_string("trail_head")?,
            checkpoints: doc.get("checkpoints")
                .and_then(|item| item.as_integer())
                .ok_or("A delivery entry has no \"checkpoints\" entry.".to_string())?,
        }),
        _ => None,
    };

    Ok(ManifestEntry {
        subject: read_string("subject")?,
        kind,
        recorded_at: doc.get("recorded_at")
            .and_then(|item| item.as_integer())
            .ok_or("A manifest entry has no \"recorded_at\" entry.".to_string())?,
        body: read_string("body")?,
        provenance,
        delivery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(subject: &str, kind: ManifestKind, body: &str) -> ManifestEntry {
        ManifestEntry {
            subject: subject.to_string(),
            kind,
            recorded_at: 100,
            body: body.to_string(),
            provenance: None,
            delivery: None,
        }
    }

    #[test]
    fn entries_round_trip_through_toml() {
        for kind in [ManifestKind::Note, ManifestKind::Approval] {
            let original = entry(&"a".repeat(64), kind, "looks good");
            let parsed = parse_entry(&entry_to_toml(&original)).unwrap();
            assert_eq!(parsed, original);
        }
    }

    #[test]
    fn provenance_entries_round_trip_through_toml() {
        let mut original = entry(&"a".repeat(64), ManifestKind::Provenance, "generated the module");
        original.provenance = Some(Provenance {
            model: "claude-opus-4-8".to_string(),
            tool: Some("claude-code".to_string()),
            session: Some("sess-123".to_string()),
            transcript: None, // an absent optional field must round-trip as None
        });

        let parsed = parse_entry(&entry_to_toml(&original)).unwrap();
        assert_eq!(parsed, original);
        assert_eq!(parsed.provenance.unwrap().model, "claude-opus-4-8");
    }

    #[test]
    fn entry_id_is_content_derived_and_stable() {
        let a = entry(&"a".repeat(64), ManifestKind::Approval, "ok");
        let same = entry(&"a".repeat(64), ManifestKind::Approval, "ok");
        let different_body = entry(&"a".repeat(64), ManifestKind::Approval, "nope");
        let different_kind = entry(&"a".repeat(64), ManifestKind::Note, "ok");

        assert_eq!(a.id(), same.id());
        assert_ne!(a.id(), different_body.id());
        assert_ne!(a.id(), different_kind.id());
    }
}
