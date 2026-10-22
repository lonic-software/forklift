//! Meta pallets as signed state trees.
//!
//! A meta pallet works the way the office already does: each parcel's tree is the
//! pallet's complete current state, so reading the state means reading the head parcel's tree
//! and nothing else. The chain is linear (every parcel has at most one parent), and a write
//! stacks one signed parcel on the head.
//!
//! The tree keeps the `.forklift/tracked/` spine (so the content guard still refuses to
//! materialize a meta parcel into a working copy), and under it sit exactly two entries:
//!
//! - `format`, a blob reading `forklift-meta 1 <kind>`;
//! - `cells/`, the state, omitted when empty. A cell is one blob at a pallet-defined path, and
//!   the path is the cell's identity.
//!
//! A cell blob is a TOML envelope: `signer` (the key id of whoever wrote the value), `schema`
//! (a short name for the value's shape) and a `[value]` table. The envelope carries no time —
//! the time is the parcel's — so one signer writing one value always produces the same bytes.
//!
//! The signer stamp is trustworthy only because every parcel passes [`verify_meta_content`]:
//! a parcel may add or change only cells stamped with its own, verified, signing key. Over a
//! linear chain that is an induction — every cell at a verified head names the key of the
//! parcel that last wrote that path.
//!
//! Divergence is reconciled without merge parcels: [`detect_conflicts`] walks base, ours and
//! theirs and names every path both sides changed differently, [`plan_replay`] plans the local
//! side's *net* change since base, and [`replay_meta_pallet`] stacks that plan onto the remote
//! head as one new parcel.
//!
//! Enforcement and sync dispatch on [`meta_rules`], a code-level list of meta pallet names,
//! never on whether a pallet happens to carry a format marker. A name not on the list has no
//! rules here, so nothing admits, replays or reads it as a meta pallet.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;
use chrono::Utc;
use toml_edit::{DocumentMut, Item, Table};
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::blob::Blob;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::model::tree_item::TreeItem;
use crate::util::audit_utils::{self, DistrustBoundaryMemo, SignatureTrust, VerifiedOffice};
use crate::util::office_utils::{KeyRecord, OfficeState, RevocationReason, TrustAnchor};
use crate::util::sign_utils::ParcelSignature;
use crate::util::{haul_utils, kv_utils, manifest_utils, object_utils, office_utils, pallet_utils, sign_utils, tag_utils};

/// The spine every meta tree keeps: `.forklift/tracked/`.
const TREE_NAME_FORKLIFT: &str = ".forklift";
const TREE_NAME_TRACKED: &str = "tracked";

/// The format marker blob, directly under the spine.
const ENTRY_NAME_FORMAT: &str = "format";

/// The state, directly under the spine (omitted when empty).
const TREE_NAME_CELLS: &str = "cells";

/// The format marker's fixed prefix and the one version this build writes and reads.
const FORMAT_PREFIX: &str = "forklift-meta";
const FORMAT_VERSION: u32 = 1;

/// The cell envelope's three fields — and the only three it may have.
const FIELD_SIGNER: &str = "signer";
const FIELD_SCHEMA: &str = "schema";
const FIELD_VALUE: &str = "value";

/// Every meta pallet name a remote stores: the office, and the four kinds below. A meta ref
/// update for any other name is refused, so a newer client's new pallet fails loudly against
/// an older remote instead of being stored unchecked.
pub const META_PALLET_NAMES: [&str; 5] = [
    office_utils::OFFICE_PALLET_NAME,
    manifest_utils::MANIFEST_PALLET_NAME,
    haul_utils::HAUL_PALLET_NAME,
    tag_utils::TAGS_PALLET_NAME,
    kv_utils::KV_PALLET_NAME,
];

/// The rules of a meta pallet on this substrate, or `None` for the office, which is on its own
/// code.
pub fn meta_rules(name: &str) -> Option<&'static dyn PalletRules> {
    match name {
        kv_utils::KV_PALLET_NAME => Some(&kv_utils::KvRules),
        manifest_utils::MANIFEST_PALLET_NAME => Some(&manifest_utils::ManifestRules),
        tag_utils::TAGS_PALLET_NAME => Some(&tag_utils::TagsRules),
        haul_utils::HAUL_PALLET_NAME => Some(&haul_utils::HaulRules),
        _ => None,
    }
}

/// Refuse a meta pallet name outside [`META_PALLET_NAMES`].
pub fn check_meta_pallet_name(name: &str) -> Result<(), String> {
    if META_PALLET_NAMES.contains(&name) {
        return Ok(());
    }

    Err(format!(
        "@{} is not a meta pallet this forklift knows. It may have been written by a newer \
        forklift; upgrade to sync it.",
        name
    ))
}

/// The kind of a meta pallet, named by its format marker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MetaKind {
    Kv,
    Manifest,
    Haul,
    Tags,
}

impl MetaKind {
    const ALL: [MetaKind; 4] = [MetaKind::Kv, MetaKind::Manifest, MetaKind::Haul, MetaKind::Tags];

    /// The kind's name, as the format marker spells it.
    pub fn as_str(&self) -> &'static str {
        match self {
            MetaKind::Kv => "kv",
            MetaKind::Manifest => "manifest",
            MetaKind::Haul => "haul",
            MetaKind::Tags => "tags",
        }
    }

    /// The exact content of this kind's `format` blob.
    pub fn format_marker(&self) -> String {
        format!("{} {} {}", FORMAT_PREFIX, FORMAT_VERSION, self.as_str())
    }
}

/// What a `format` blob says.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatRead {
    /// A marker this build writes, byte for byte.
    Known(MetaKind),

    /// Anything else (shortened for messages): a newer version, an unknown kind, or bytes
    /// that are no marker at all. Never guessed at.
    Unknown(String),
}

/// Read a `format` blob. The match is exact — no trimming, no case folding — so a marker this
/// build did not write can never be mistaken for one it did.
pub fn parse_format(bytes: &[u8]) -> FormatRead {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return FormatRead::Unknown("(not UTF-8)".to_string()),
    };

    for kind in MetaKind::ALL {
        if text == kind.format_marker() {
            return FormatRead::Known(kind);
        }
    }

    FormatRead::Unknown(text.chars().take(80).collect())
}

/// Check one component of a cell path: ASCII letters, digits, `.`, `_` and `-` only, and never
/// a leading `.` or `-` (which also rules out `.`, `..` and any nested `.forklift`).
pub fn validate_component(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("A metadata path component may not be empty.".to_string());
    }

    if name.starts_with('.') || name.starts_with('-') {
        return Err(format!(
            "The metadata path component \"{}\" may not begin with \".\" or \"-\".", name
        ));
    }

    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err(format!(
            "The metadata path component \"{}\" may contain only ASCII letters, digits, \".\", \
            \"_\" and \"-\".",
            name
        ));
    }

    Ok(())
}

/// Check a whole cell path (relative to `cells/`, components joined by `/`).
pub fn validate_cell_path(path: &str) -> Result<(), String> {
    path.split('/').try_for_each(validate_component)
}

/// A parsed cell envelope.
#[derive(Clone, Debug)]
pub struct CellEnvelope {
    /// The key id of whoever wrote this value.
    pub signer: String,

    /// The value's shape, for example `kv.text 1`.
    pub schema: String,

    /// The payload.
    pub value: Table,
}

/// Encode a cell envelope. Deterministic: the same signer, schema and value always give the
/// same bytes, which is what lets the sync walk read identical cells as agreement.
pub fn encode_cell(signer: &str, schema: &str, value: &Table) -> Vec<u8> {
    let mut document = DocumentMut::new();
    document.insert(FIELD_SIGNER, toml_edit::value(signer));
    document.insert(FIELD_SCHEMA, toml_edit::value(schema));

    let mut value = value.clone();
    value.set_implicit(false); // An empty payload still writes its `[value]` header.
    document.insert(FIELD_VALUE, Item::Table(value));

    document.to_string().into_bytes()
}

/// Parse a cell envelope, strictly: exactly the three fields, a 64-hex signer, a non-empty
/// schema string and a `[value]` table.
pub fn parse_cell(bytes: &[u8]) -> Result<CellEnvelope, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "the cell is not valid UTF-8".to_string())?;

    let document: DocumentMut = text.parse()
        .map_err(|error: toml_edit::TomlError| format!("the cell is not valid TOML ({})", error.message()))?;

    if let Some((key, _)) = document.iter()
        .find(|(key, _)| ![FIELD_SIGNER, FIELD_SCHEMA, FIELD_VALUE].contains(key)) {
        return Err(format!("the cell has an unexpected field \"{}\"", key));
    }

    let signer = match document.get(FIELD_SIGNER) {
        Some(item) => item.as_str().ok_or("the cell's signer is not a string".to_string())?,
        None => return Err("the cell names no signer".to_string()),
    };

    if signer.len() != 64 || !signer.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(format!("the cell's signer \"{}\" is not a key id", signer));
    }

    let schema = match document.get(FIELD_SCHEMA) {
        Some(item) => item.as_str().ok_or("the cell's schema is not a string".to_string())?,
        None => return Err("the cell names no schema".to_string()),
    };

    if schema.is_empty() {
        return Err("the cell's schema is empty".to_string());
    }

    let value = match document.get(FIELD_VALUE) {
        Some(Item::Table(table)) => table.clone(),
        Some(_) => return Err("the cell's value is not a table".to_string()),
        None => return Err("the cell has no value table".to_string()),
    };

    Ok(CellEnvelope { signer: signer.to_string(), schema: schema.to_string(), value })
}

/// The per-pallet half of the content check, and of reading. Each meta pallet supplies
/// one; the substrate never interprets a cell's value itself.
pub trait PalletRules {
    /// The kind this pallet's format marker must name.
    fn kind(&self) -> MetaKind;

    /// Whether this build understands a cell schema. A cell whose schema it does not is kept
    /// and surfaced as unreadable, never dropped.
    fn knows_schema(&self, schema: &str) -> bool;

    /// The pallet's own rules over one parcel's changes (who may write what, what may never
    /// change). Runs after the substrate's own checks have passed.
    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String>;

    /// The cell path a resolution names, from how the operator spells it (`lower --keep`).
    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        validate_cell_path(spelled)?;

        Ok(spelled.to_string())
    }

    /// A cell path as the operator spells it — the inverse of [`PalletRules::resolution_path`].
    fn display_path(&self, path: &str) -> String {
        path.to_string()
    }

    /// A readable cell's value, for showing to the operator.
    fn display_value(&self, envelope: &CellEnvelope) -> String {
        envelope.value.to_string()
    }
}

/// One cell path whose blob differs between two trees.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CellChange {
    /// The path, relative to `cells/`.
    pub path: String,

    /// The blob before, or `None` when the path held no cell.
    pub before: Option<String>,

    /// The blob after, or `None` when the cell is gone.
    pub after: Option<String>,
}

/// Why a content check runs, which decides whether rules about the signer's current role apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Admission {
    /// New parcels are being admitted to a head: a local write, a replay, or the server moving
    /// its head. A role rule is checked against the office as it stands now.
    Admit,

    /// History a server already admitted is being adopted. A role rule is not checked: the
    /// office may have changed since, and a client does not verify the office's own role rules
    /// either, so re-checking would only wedge every client behind a since-demoted signer.
    Audit,
}

/// Everything a pallet's rules see about one parcel.
pub struct ParcelChange<'a> {
    /// The parcel's hash.
    pub parcel: &'a str,

    /// The verified key that signed it.
    pub signer: &'a KeyRecord,

    /// The office it was verified against.
    pub office: &'a OfficeState,

    /// The parent's `cells/` tree (`None` for a root parcel or an empty parent).
    pub parent_cells: Option<&'a str>,

    /// The parcel's own `cells/` tree (`None` when empty).
    pub cells: Option<&'a str>,

    /// Every cell the parcel adds, changes or removes, relative to its parent.
    pub changes: &'a [CellChange],

    /// Why the check runs.
    pub admission: Admission,
}

/// One cell as a reader sees it. A bad cell never stops the others from reading.
#[derive(Clone, Debug)]
pub enum CellRead {
    /// A cell this build understands.
    Readable(CellEnvelope),

    /// A well-formed cell whose schema this build does not know — written by a newer forklift.
    Unknown {
        signer: String,
        schema: String,
    },

    /// A cell whose envelope does not parse.
    Malformed(String),
}

/// The state at one head of a meta pallet.
#[derive(Debug)]
pub struct MetaState {
    pub kind: MetaKind,

    /// Every cell, by path relative to `cells/`.
    pub cells: BTreeMap<String, CellRead>,
}

/// One side's value at a tree position. Absence is a value, not an `Option` around one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Entry {
    Absent,
    Blob(String),
    Tree(String),
}

impl Entry {
    fn is_blob(&self) -> bool {
        matches!(self, Entry::Blob(_))
    }
}

/// One loaded tree level: entry name to value.
type Level = BTreeMap<String, Entry>;

/// What the walk decided at one path. Diagnostic: the conflicts list is the verdict.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// Ours equals theirs (both absent included): agreement, not descended into.
    TakeIdentical,

    /// Only theirs changed since base.
    TakeTheirs,

    /// Only ours changed since base.
    TakeOurs,

    /// No present side is a cell: descended into.
    Recurse,

    /// Both changed, differently.
    Conflict,
}

/// A path both sides changed differently since base, with the three values.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MetaConflict {
    pub path: String,
    pub base: Entry,
    pub ours: Entry,
    pub theirs: Entry,
}

/// The walk's result: every decision it made, and every conflicted path.
pub struct WalkOutcome {
    pub decisions: Vec<(String, Decision)>,
    pub conflicts: Vec<MetaConflict>,
}

/// Which side a resolution keeps at a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeepSide {
    /// Replay the local value on top of the remote one.
    Ours,

    /// Drop every local change at or beneath the path.
    Theirs,
}

/// Why a replay produced no parcel.
#[derive(Debug)]
pub enum ReplayError {
    /// Paths both sides changed differently, none of them resolved. Nothing is replayed.
    Conflicts(Vec<MetaConflict>),

    /// Anything else: a stale resolution, a cell that cannot honestly be re-signed, a format
    /// this build cannot read, or a store error.
    Refused(String),
}

impl From<String> for ReplayError {
    fn from(message: String) -> Self {
        ReplayError::Refused(message)
    }
}

/// One edit to a meta pallet's state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CellEdit {
    /// Put the (already stored) cell blob at the path.
    Set {
        path: String,
        blob: String,
    },

    /// Remove the cell at the path, if there is one.
    Unset {
        path: String,
    },
}

impl CellEdit {
    fn path(&self) -> &str {
        match self {
            CellEdit::Set { path, .. } | CellEdit::Unset { path } => path,
        }
    }
}

/// Encode and store a cell, and return the edit that sets it.
pub fn set_cell(path: &str, signer: &str, schema: &str, value: &Table) -> Result<CellEdit, String> {
    Ok(CellEdit::Set {
        path: path.to_string(),
        blob: store_blob(&encode_cell(signer, schema, value))?,
    })
}

/// Who signs a new meta parcel. Injected, so a write never assumes where the private key is.
pub struct MetaSigner<'a> {
    /// The id of the signing key; every cell a parcel writes must carry it.
    pub key_id: String,

    sign: Box<dyn Fn(&str) -> Result<ParcelSignature, String> + 'a>,
}

impl<'a> MetaSigner<'a> {
    /// Sign with a private key stored on this machine.
    pub fn local(key_id: &str) -> MetaSigner<'static> {
        let owned = key_id.to_string();

        MetaSigner {
            key_id: key_id.to_string(),
            sign: Box::new(move |hash| sign_utils::sign_parcel_hash(&owned, hash)),
        }
    }

    /// Sign with any function of the parcel hash.
    pub fn new(key_id: &str,
               sign: impl Fn(&str) -> Result<ParcelSignature, String> + 'a) -> MetaSigner<'a> {
        MetaSigner { key_id: key_id.to_string(), sign: Box::new(sign) }
    }
}

/// The hash of the empty tree, which reads as absence everywhere in this module.
fn empty_tree_hash() -> &'static str {
    static EMPTY: OnceLock<String> = OnceLock::new();

    EMPTY.get_or_init(|| {
        LooseObjectBuilder::build_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree)).hash
    })
}

/// Canonical absence: an empty subtree is the same value as no subtree, so two writers that
/// leave and prune an emptied subject agree.
fn normalize(entry: Entry) -> Entry {
    match entry {
        Entry::Tree(hash) if hash == empty_tree_hash() => Entry::Absent,
        other => other,
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", path, name)
    }
}

fn at_or_under(path: &str, prefix: &str) -> bool {
    path == prefix || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

fn root_entry(hash: Option<&str>) -> Entry {
    hash.map_or(Entry::Absent, |hash| Entry::Tree(hash.to_string()))
}

fn store_blob(bytes: &[u8]) -> Result<String, String> {
    let mut object = LooseObjectBuilder::build_blob(&Blob { content: bytes.to_vec() });
    object.store()?;

    Ok(object.hash)
}

fn store_level(level: &Level) -> Result<String, String> {
    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

    for (name, entry) in level {
        match entry {
            Entry::Blob(hash) => tree.add_child(TreeItem::new(name.clone(), hash.clone(), DirEntryType::Normal)),
            Entry::Tree(hash) => tree.add_child(TreeItem::new(name.clone(), hash.clone(), DirEntryType::Tree)),
            Entry::Absent => {}
        }
    }

    let mut object = LooseObjectBuilder::build_tree(&tree);
    object.store()?;

    Ok(object.hash)
}

/// Load one level of a cells tree. Every name must be a valid component and every file a
/// plain cell blob. `strict` (the content check) also refuses an explicit empty subtree, which
/// a writer must prune instead.
fn load_level(path: &str, hash: &str, strict: bool) -> Result<Level, String> {
    let tree = object_utils::load_tree(hash)?;
    let mut level = Level::new();

    for (name, item) in tree.get_subtrees() {
        validate_component(name).map_err(|why| format!("At {}: {}", join(path, name), why))?;

        if strict && item.hash == empty_tree_hash() {
            return Err(format!("{} is an empty subtree, which must be pruned instead.", join(path, name)));
        }

        level.insert(name.clone(), Entry::Tree(item.hash.clone()));
    }

    for (name, item) in tree.get_files() {
        validate_component(name).map_err(|why| format!("At {}: {}", join(path, name), why))?;

        if item.item_type != DirEntryType::Normal {
            return Err(format!("{} is a {:?} entry, not a cell.", join(path, name), item.item_type));
        }

        level.insert(name.clone(), Entry::Blob(item.hash.clone()));
    }

    Ok(level)
}

/// The level beneath an entry: its tree's, or nothing for a cell or an absence.
fn level_of(path: &str,
            entry: &Entry,
            load: &mut dyn FnMut(&str, &str) -> Result<Level, String>) -> Result<Level, String> {
    match entry {
        Entry::Tree(hash) => load(path, hash),
        _ => Ok(Level::new()),
    }
}

fn union_names<'a>(levels: &[&'a Level]) -> BTreeSet<&'a String> {
    levels.iter().flat_map(|level| level.keys()).collect()
}

/// The value at a path in a cells tree.
fn resolve_entry(cells: Option<&str>, path: &str) -> Result<Entry, String> {
    let mut current = normalize(root_entry(cells));
    let mut walked = String::new();

    for component in path.split('/').filter(|component| !component.is_empty()) {
        current = match &current {
            Entry::Tree(hash) => load_level(&walked, hash, false)?
                .remove(component)
                .unwrap_or(Entry::Absent),
            _ => Entry::Absent,
        };

        walked = join(&walked, component);
        current = normalize(current);
    }

    Ok(current)
}

/// Every cell path whose blob differs between `before` and `after`, at cell granularity: a
/// subtree with the same hash on both sides is never descended into. A cell replaced by a
/// subtree (or the reverse) is the cell's removal plus every cell of the subtree.
fn diff_entries(path: &str,
                before: Entry,
                after: Entry,
                load: &mut dyn FnMut(&str, &str) -> Result<Level, String>,
                out: &mut Vec<CellChange>) -> Result<(), String> {
    let before = normalize(before);
    let after = normalize(after);

    if before == after {
        return Ok(());
    }

    if !matches!(before, Entry::Tree(_)) && !matches!(after, Entry::Tree(_)) {
        out.push(CellChange { path: path.to_string(), before: blob_hash(&before), after: blob_hash(&after) });
        return Ok(());
    }

    if let Entry::Blob(hash) = &before {
        out.push(CellChange { path: path.to_string(), before: Some(hash.clone()), after: None });
    }

    let before_level = level_of(path, &before, load)?;
    let after_level = level_of(path, &after, load)?;

    for name in union_names(&[&before_level, &after_level]) {
        diff_entries(
            &join(path, name),
            before_level.get(name).cloned().unwrap_or(Entry::Absent),
            after_level.get(name).cloned().unwrap_or(Entry::Absent),
            load,
            out,
        )?;
    }

    if let Entry::Blob(hash) = &after {
        out.push(CellChange { path: path.to_string(), before: None, after: Some(hash.clone()) });
    }

    Ok(())
}

fn blob_hash(entry: &Entry) -> Option<String> {
    match entry {
        Entry::Blob(hash) => Some(hash.clone()),
        _ => None,
    }
}

/// The net change from one cells tree to another.
pub fn net_diff(before: Option<&str>, after: Option<&str>) -> Result<Vec<CellChange>, String> {
    let mut changes = Vec::new();
    diff_entries("", root_entry(before), root_entry(after), &mut |path, hash| load_level(path, hash, false), &mut changes)?;

    Ok(changes)
}

/// Apply edits to a cells tree, returning the new tree (`None` when it ends up empty). Only the
/// spine above an edited path is rebuilt — every other subtree keeps its hash — and emptied
/// subtrees are pruned. A path may be named once; setting a cell where a subtree is, or
/// beneath a cell, is refused.
pub fn apply_edits(cells: Option<&str>, edits: &[CellEdit]) -> Result<Option<String>, String> {
    let mut seen = BTreeSet::new();

    for edit in edits {
        validate_cell_path(edit.path())?;

        if !seen.insert(edit.path()) {
            return Err(format!("The cell {} is edited twice in one write.", edit.path()));
        }
    }

    let split: Vec<Vec<&str>> = edits.iter().map(|edit| edit.path().split('/').collect()).collect();
    let refs: Vec<(&[&str], &CellEdit)> = split.iter().map(Vec::as_slice).zip(edits).collect();

    apply_level("", cells.map(str::to_string), &refs)
}

fn apply_level(path: &str,
               hash: Option<String>,
               edits: &[(&[&str], &CellEdit)]) -> Result<Option<String>, String> {
    let mut level = match &hash {
        Some(hash) => load_level(path, hash, false)?,
        None => Level::new(),
    };

    let mut groups: BTreeMap<&str, Vec<(&[&str], &CellEdit)>> = BTreeMap::new();

    for (components, edit) in edits {
        groups.entry(components[0]).or_default().push((&components[1..], *edit));
    }

    for (name, group) in groups {
        let child_path = join(path, name);
        let mut current = normalize(level.remove(name).unwrap_or(Entry::Absent));

        // Unsets of this exact path first, then everything beneath it, then sets of it: a cell
        // can become a subtree (unset, then fill) and a subtree a cell (empty, then set) in
        // one write.
        for (rest, edit) in &group {
            if rest.is_empty() && matches!(edit, CellEdit::Unset { .. }) {
                if let Entry::Tree(_) = current {
                    return Err(format!("{} holds cells beneath it; it is not itself a cell to unset.", child_path));
                }

                current = Entry::Absent;
            }
        }

        let deeper: Vec<(&[&str], &CellEdit)> = group.iter()
            .filter(|(rest, _)| !rest.is_empty())
            .cloned()
            .collect();

        // Unsetting beneath a cell removes nothing, as unsetting any absent path does.
        let only_unsets = deeper.iter().all(|(_, edit)| matches!(edit, CellEdit::Unset { .. }));

        if !deeper.is_empty() && !(only_unsets && matches!(current, Entry::Blob(_))) {
            let subtree = match &current {
                Entry::Blob(_) => return Err(format!("{} is a cell, so nothing can be set beneath it.", child_path)),
                Entry::Tree(hash) => Some(hash.clone()),
                Entry::Absent => None,
            };

            current = match apply_level(&child_path, subtree, &deeper)? {
                Some(hash) => Entry::Tree(hash),
                None => Entry::Absent,
            };
        }

        for (rest, edit) in &group {
            if let (true, CellEdit::Set { blob, .. }) = (rest.is_empty(), edit) {
                if let Entry::Tree(_) = current {
                    return Err(format!("{} holds cells beneath it, so it cannot itself be a cell.", child_path));
                }

                current = Entry::Blob(blob.clone());
            }
        }

        if current != Entry::Absent {
            level.insert(name.to_string(), current);
        }
    }

    level.retain(|_, entry| normalize(entry.clone()) != Entry::Absent);

    if level.is_empty() {
        return Ok(None);
    }

    Ok(Some(store_level(&level)?))
}

/// A meta tree's spine, read.
struct Spine {
    /// The `format` blob's hash.
    format: String,

    /// The `cells/` tree, if any.
    cells: Option<String>,
}

/// Why a tree is not a meta tree.
enum SpineFault {
    /// No format marker at all — the shape every pallet written before this format has.
    NoFormat,

    /// A marker, but a spine this build never writes.
    Malformed(String),
}

fn read_spine(tree_hash: &str) -> Result<Result<Spine, SpineFault>, String> {
    let root = object_utils::load_tree(tree_hash)?;

    // An older forklift's meta parcel held its records under `.forklift/tracked/`, or nothing
    // at all for a join. Only those shapes read as older; any other tree is a corrupt spine.
    if entry_count(&root) == 0 {
        return Ok(Err(SpineFault::NoFormat));
    }

    let no_spine = || Ok(Err(SpineFault::Malformed("its tree has no .forklift/tracked/ spine".to_string())));

    let Some(forklift) = subtree_named(&root, TREE_NAME_FORKLIFT) else {
        return no_spine();
    };
    let forklift = object_utils::load_tree(&forklift)?;

    let Some(tracked) = subtree_named(&forklift, TREE_NAME_TRACKED) else {
        return no_spine();
    };
    let tracked = object_utils::load_tree(&tracked)?;

    let Some((_, format)) = tracked.get_files().find(|(name, _)| name.as_str() == ENTRY_NAME_FORMAT) else {
        if subtree_named(&tracked, ENTRY_NAME_FORMAT).is_some() {
            return Ok(Err(SpineFault::Malformed("its format marker is a subtree, not a blob".to_string())));
        }

        if entry_count(&root) != 1 || entry_count(&forklift) != 1 {
            return Ok(Err(SpineFault::Malformed(
                "its tree holds entries besides .forklift/tracked/".to_string()
            )));
        }

        return Ok(Err(SpineFault::NoFormat));
    };

    if entry_count(&root) != 1 || entry_count(&forklift) != 1 {
        return Ok(Err(SpineFault::Malformed(
            "its tree holds entries besides .forklift/tracked/".to_string()
        )));
    }

    if format.item_type != DirEntryType::Normal {
        return Ok(Err(SpineFault::Malformed("its format marker is not a plain blob".to_string())));
    }

    let cells = subtree_named(&tracked, TREE_NAME_CELLS);

    if entry_count(&tracked) != 1 + usize::from(cells.is_some()) {
        return Ok(Err(SpineFault::Malformed(
            "its tree holds entries besides the format marker and cells/".to_string()
        )));
    }

    if cells.as_deref() == Some(empty_tree_hash()) {
        return Ok(Err(SpineFault::Malformed("its cells/ tree is empty and must be omitted".to_string())));
    }

    Ok(Ok(Spine { format: format.hash.clone(), cells }))
}

fn subtree_named(tree: &TreeItem, name: &str) -> Option<String> {
    tree.get_subtrees()
        .find(|(entry, _)| entry.as_str() == name)
        .map(|(_, item)| item.hash.clone())
}

fn entry_count(tree: &TreeItem) -> usize {
    tree.get_subtrees().count() + tree.get_files().count()
}

/// Read a parcel's spine as a pallet of the expected kind, refusing the whole pallet loudly on
/// anything this build cannot read. It never falls back.
fn read_pallet_spine(pallet: &str, parcel_hash: &str, expected: MetaKind) -> Result<Spine, String> {
    let parcel = object_utils::load_parcel(parcel_hash)?;

    let spine = match read_spine(&parcel.tree_hash)? {
        Ok(spine) => spine,
        Err(SpineFault::NoFormat) => return Err(format!(
            "The meta pallet @{} has no format marker at {}: it was written by an older forklift, \
            and this one cannot read it.",
            pallet, parcel_hash
        )),
        Err(SpineFault::Malformed(why)) => return Err(format!(
            "The meta pallet @{} is malformed at {}: {}.", pallet, parcel_hash, why
        )),
    };

    check_format(pallet, parcel_hash, &spine, expected)?;

    Ok(spine)
}

fn check_format(pallet: &str, parcel_hash: &str, spine: &Spine, expected: MetaKind) -> Result<(), String> {
    match parse_format(&object_utils::load_blob(&spine.format)?.content) {
        FormatRead::Known(kind) if kind == expected => Ok(()),
        FormatRead::Known(kind) => Err(format!(
            "The meta pallet @{} holds {} metadata at {}, not {}.",
            pallet, kind.as_str(), parcel_hash, expected.as_str()
        )),
        FormatRead::Unknown(marker) => Err(format!(
            "The meta pallet @{} is in the format \"{}\" at {}, which this forklift does not know: \
            it was written by a newer forklift.",
            pallet, marker, parcel_hash
        )),
    }
}

/// Refuse a value table holding any field outside `allowed`.
pub fn value_fields_only(table: &Table, allowed: &[&str]) -> Result<(), String> {
    match table.iter().find(|(field, _)| !allowed.contains(field)) {
        Some((field, _)) => Err(format!("has an unexpected field \"{}\"", field)),
        None => Ok(()),
    }
}

/// A value table's string field, which must be present.
pub fn value_string(table: &Table, field: &str) -> Result<String, String> {
    value_optional_string(table, field)?.ok_or(format!("has no {} field", field))
}

/// A value table's string field, `None` when absent. A present field that is not a string is
/// an error, never an absence.
pub fn value_optional_string(table: &Table, field: &str) -> Result<Option<String>, String> {
    match table.get(field) {
        Some(Item::Value(item)) => item.as_str()
            .map(|text| Some(text.to_string()))
            .ok_or(format!("has a {} field that is not a string", field)),
        Some(_) => Err(format!("has a {} field that is not a string", field)),
        None => Ok(None),
    }
}

/// A value table's integer field, which must be present.
pub fn value_integer(table: &Table, field: &str) -> Result<i64, String> {
    match table.get(field) {
        Some(Item::Value(item)) => item.as_integer().ok_or(format!("has a {} field that is not an integer", field)),
        Some(_) => Err(format!("has a {} field that is not an integer", field)),
        None => Err(format!("has no {} field", field)),
    }
}

/// Read the state at a head, tolerantly per cell: an unknown schema or a malformed envelope is
/// reported as that cell's row and never stops the rest. A missing, unknown or mismatched
/// format refuses the whole pallet.
pub fn read_meta_state(pallet: &str, head: &str, rules: &dyn PalletRules) -> Result<MetaState, String> {
    let spine = read_pallet_spine(pallet, head, rules.kind())?;
    let mut cells = BTreeMap::new();

    if let Some(root) = &spine.cells {
        collect_cells("", root, rules, &mut cells)?;
    }

    Ok(MetaState { kind: rules.kind(), cells })
}

/// [`read_meta_state`] at the pallet's local head; `None` while the pallet is unborn.
pub fn read_meta_head(pallet: &str, rules: &dyn PalletRules) -> Result<Option<MetaState>, String> {
    match pallet_utils::get_meta_pallet_head(pallet)? {
        Some(head) => read_meta_state(pallet, &head, rules).map(Some),
        None => Ok(None),
    }
}

/// The cell at one path at a head, or `None` when the path holds no cell. Only the path's own
/// levels are loaded.
pub fn read_cell(pallet: &str, head: &str, rules: &dyn PalletRules, path: &str) -> Result<Option<CellRead>, String> {
    let spine = read_pallet_spine(pallet, head, rules.kind())?;

    read_cell_in(spine.cells.as_deref(), path, rules)
}

/// The `cells/` tree at a head (`None` when the state is empty).
pub fn cells_at(pallet: &str, head: &str, rules: &dyn PalletRules) -> Result<Option<String>, String> {
    Ok(read_pallet_spine(pallet, head, rules.kind())?.cells)
}

/// Read one stored cell blob as [`read_meta_state`] would.
pub fn read_cell_blob(blob: &str, rules: &dyn PalletRules) -> Result<CellRead, String> {
    Ok(classify_cell(&object_utils::load_blob(blob)?.content, rules))
}

/// The cell at a path in a `cells/` tree read with [`cells_at`], or `None` when none is there.
pub fn read_cell_in(cells: Option<&str>, path: &str, rules: &dyn PalletRules) -> Result<Option<CellRead>, String> {
    match resolve_entry(cells, path)? {
        Entry::Blob(blob) => Ok(Some(classify_cell(&object_utils::load_blob(&blob)?.content, rules))),
        _ => Ok(None),
    }
}

/// One parcel that changed a cell, as [`cell_history`] reports it.
#[derive(Debug)]
pub struct CellHistoryEntry {
    /// The parcel that changed the cell.
    pub parcel: String,

    /// The cell after the parcel, or `None` when the parcel removed it.
    pub after: Option<CellRead>,
}

/// Every parcel from `head` back to the pallet's root that changed the cell at `path`, newest
/// first. It walks the whole chain, so it runs only when asked for.
pub fn cell_history(pallet: &str,
                    head: &str,
                    rules: &dyn PalletRules,
                    path: &str) -> Result<Vec<CellHistoryEntry>, String> {
    let mut history = Vec::new();
    let mut cursor = Some(head.to_string());
    let mut newer: Option<(String, Entry)> = None;

    while let Some(hash) = cursor {
        let parcel = object_utils::load_parcel(&hash)?;

        if parcel.parents.len() > 1 {
            return Err(format!(
                "Parcel {} on @{} has {} parents, but a meta pallet is linear.",
                hash, pallet, parcel.parents.len()
            ));
        }

        let entry = resolve_entry(read_pallet_spine(pallet, &hash, rules.kind())?.cells.as_deref(), path)?;

        if let Some((child, child_entry)) = newer.take() {
            if child_entry != entry {
                history.push(history_entry(child, &child_entry, rules)?);
            }
        }

        cursor = parcel.parents.first().cloned();
        newer = Some((hash, entry));
    }

    if let Some((root, entry)) = newer {
        if entry != Entry::Absent {
            history.push(history_entry(root, &entry, rules)?);
        }
    }

    Ok(history)
}

fn history_entry(parcel: String, entry: &Entry, rules: &dyn PalletRules) -> Result<CellHistoryEntry, String> {
    let after = match entry {
        Entry::Blob(blob) => Some(classify_cell(&object_utils::load_blob(blob)?.content, rules)),
        _ => None,
    };

    Ok(CellHistoryEntry { parcel, after })
}

fn collect_cells(path: &str,
                 hash: &str,
                 rules: &dyn PalletRules,
                 out: &mut BTreeMap<String, CellRead>) -> Result<(), String> {
    for (name, entry) in load_level(path, hash, false)? {
        let child = join(path, &name);

        match entry {
            Entry::Tree(subtree) => collect_cells(&child, &subtree, rules, out)?,
            Entry::Blob(blob) => {
                let bytes = object_utils::load_blob(&blob)?.content;
                out.insert(child, classify_cell(&bytes, rules));
            }
            Entry::Absent => {}
        }
    }

    Ok(())
}

fn classify_cell(bytes: &[u8], rules: &dyn PalletRules) -> CellRead {
    match parse_cell(bytes) {
        Ok(envelope) if rules.knows_schema(&envelope.schema) => CellRead::Readable(envelope),
        Ok(envelope) => CellRead::Unknown { signer: envelope.signer, schema: envelope.schema },
        Err(why) => CellRead::Malformed(why),
    }
}

/// Store a meta tree: the spine, the given format marker and the cells.
fn store_meta_tree(format_marker: &[u8], cells: Option<&str>) -> Result<String, String> {
    let mut tracked = Level::new();
    tracked.insert(ENTRY_NAME_FORMAT.to_string(), Entry::Blob(store_blob(format_marker)?));

    if let Some(cells) = cells {
        tracked.insert(TREE_NAME_CELLS.to_string(), Entry::Tree(cells.to_string()));
    }

    let forklift = Level::from([(TREE_NAME_TRACKED.to_string(), Entry::Tree(store_level(&tracked)?))]);
    let root = Level::from([(TREE_NAME_FORKLIFT.to_string(), Entry::Tree(store_level(&forklift)?))]);

    store_level(&root)
}

/// Build, store and sign one parcel over a tree. It does not move any head.
fn stack_signed_parcel(tree_hash: String,
                       parents: Vec<String>,
                       actor: &Operator,
                       description: &str,
                       signer: &MetaSigner) -> Result<String, String> {
    let timestamp = Utc::now();

    let parcel = Parcel {
        tree_hash,
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
        description: Some(description.to_string()),
    };

    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store()?;

    let signature = (signer.sign)(&object.hash)?;

    if signature.key_id != signer.key_id {
        return Err(format!(
            "The meta parcel was to be signed with key {}, but key {} signed it.",
            signer.key_id, signature.key_id
        ));
    }

    sign_utils::store_parcel_signature(&object.hash, &signature)?;

    Ok(object.hash)
}

/// Apply edits to a meta pallet's local head and stack the result as one signed parcel, then
/// move the head. The new parcel passes [`verify_meta_content`] before the head moves, so a
/// write can never leave a head its own reader would refuse — and the head moves only while the
/// office it was checked against still stands (its head and the trust anchor), so a key retired
/// or a re-genesis in another bay meanwhile refuses the write rather than landing one. Writes
/// edit only the paths they name: every other cell, including ones this build cannot read, is
/// carried through byte for byte. The caller holds the warehouse lock.
///
/// # Returns
/// * `Ok(Some(hash))` - The new head.
/// * `Ok(None)`       - The edits changed nothing, so nothing was stacked.
/// * `Err(String)`    - If the head cannot be read, an edit is refused, the parcel fails
///                      the content check, or the head or the office moved meanwhile.
pub fn stack_meta_edits(pallet: &str,
                        rules: &dyn PalletRules,
                        office: &VerifiedOffice,
                        edits: &[CellEdit],
                        actor: &Operator,
                        description: &str,
                        signer: &MetaSigner) -> Result<Option<String>, String> {
    let head = pallet_utils::get_meta_pallet_head(pallet)?;

    let cells = match &head {
        Some(head) => read_pallet_spine(pallet, head, rules.kind())?.cells,
        None => None,
    };

    let new_cells = apply_edits(cells.as_deref(), edits)?;

    if new_cells == cells {
        return Ok(None);
    }

    let tree = store_meta_tree(rules.kind().format_marker().as_bytes(), new_cells.as_deref())?;
    let parcel = stack_signed_parcel(tree, head.iter().cloned().collect(), actor, description, signer)?;

    verify_meta_content(pallet, rules, office, Admission::Admit, head.as_deref(), &parcel)?;
    pallet_utils::update_meta_pallet_head(pallet, head.as_deref(), &parcel, &office.preconditions())?;

    Ok(Some(parcel))
}

/// The content check: every parcel from `old_head` (exclusive; `None` checks from the root) to
/// `new_head` must
///
/// - (a) have at most one parent, and descend from `old_head`;
/// - (b) carry a signature that verifies against its key in the office, and, if that key is
///   revoked, lie inside the revocation's distrust boundary;
/// - (c) stamp every cell it adds or changes, relative to its parent (the empty tree for a
///   root), with that same key;
/// - (d) keep its parent's format marker, which must be the one these rules name;
/// - (e) write only envelopes that parse, in a canonical tree (valid names, plain cell blobs,
///   no empty subtrees);
/// - (f) satisfy the pallet's own rules.
///
/// Only subtrees whose hash differs from the parent's at the same path are examined. A subtree
/// is never skipped because the same hash was verified somewhere else: a copy to a new path is
/// a change, and its stamps must be the copier's.
///
/// `old_head` is a head that already passed this check.
pub fn verify_meta_content(pallet: &str,
                           rules: &dyn PalletRules,
                           office: &OfficeState,
                           admission: Admission,
                           old_head: Option<&str>,
                           new_head: &str) -> Result<(), String> {
    let mut chain: Vec<(String, Parcel)> = Vec::new();
    let mut cursor = new_head.to_string();

    loop {
        if Some(cursor.as_str()) == old_head {
            break;
        }

        let parcel = object_utils::load_parcel(&cursor)?;

        if parcel.parents.len() > 1 {
            return Err(format!(
                "Parcel {} on @{} has {} parents. A meta pallet is linear: every parcel has at \
                most one parent.",
                cursor, pallet, parcel.parents.len()
            ));
        }

        let parent = parcel.parents.first().cloned();
        chain.push((cursor, parcel));

        match parent {
            Some(parent) => cursor = parent,
            None if old_head.is_some() => return Err(format!(
                "The new @{} head {} does not descend from the current head {}.",
                pallet, new_head, old_head.unwrap()
            )),
            None => break,
        }
    }

    chain.reverse();

    let mut parent_spine = match (old_head, chain.is_empty()) {
        (Some(old_head), false) => Some(read_pallet_spine(pallet, old_head, rules.kind())?),
        _ => None,
    };

    let mut distrust_boundaries = DistrustBoundaryMemo::new();

    for (hash, parcel) in &chain {
        let signer = verify_meta_signature(pallet, hash, office, &mut distrust_boundaries)?;

        let spine = match read_spine(&parcel.tree_hash)? {
            Ok(spine) => spine,
            Err(SpineFault::NoFormat) => return Err(format!(
                "Parcel {} on @{} has no format marker.", hash, pallet
            )),
            Err(SpineFault::Malformed(why)) => return Err(format!(
                "Parcel {} on @{} is malformed: {}.", hash, pallet, why
            )),
        };

        if let Some(parent) = &parent_spine {
            if parent.format != spine.format {
                return Err(format!(
                    "Parcel {} on @{} changes the pallet's format marker. A meta pallet keeps \
                    its format for its whole history.",
                    hash, pallet
                ));
            }
        }

        check_format(pallet, hash, &spine, rules.kind())?;

        let parent_cells = parent_spine.as_ref().and_then(|parent| parent.cells.clone());
        let mut changes = Vec::new();

        diff_entries(
            "",
            root_entry(parent_cells.as_deref()),
            root_entry(spine.cells.as_deref()),
            &mut |path, hash| load_level(path, hash, true),
            &mut changes,
        ).map_err(|why| format!("Parcel {} on @{} is malformed: {}", hash, pallet, why))?;

        for change in &changes {
            let Some(after) = &change.after else { continue };

            let envelope = parse_cell(&object_utils::load_blob(after)?.content).map_err(|why| format!(
                "Parcel {} on @{} writes a malformed cell at {}: {}.", hash, pallet, change.path, why
            ))?;

            if envelope.signer != signer.key_id {
                return Err(format!(
                    "Parcel {} on @{} writes the cell {} stamped with key {}, but the parcel is \
                    signed with key {}. A parcel may only write cells stamped with its own \
                    signing key.",
                    hash, pallet, change.path, envelope.signer, signer.key_id
                ));
            }
        }

        rules.check(&ParcelChange {
            parcel: hash,
            signer,
            office,
            parent_cells: parent_cells.as_deref(),
            cells: spine.cells.as_deref(),
            changes: &changes,
            admission,
        }).map_err(|why| format!("Parcel {} on @{} breaks a rule of the pallet: {}", hash, pallet, why))?;

        parent_spine = Some(spine);
    }

    Ok(())
}

/// The content check for a meta pallet's update on a store that holds the office: the
/// office chain must verify against the anchor, and the pallet's new parcels against that
/// office. A meta pallet exists only under an office, so a missing anchor or an unborn
/// office refuses rather than skipping the check. Returns the verified office.
pub fn verify_meta_update(pallet: &str,
                               rules: &dyn PalletRules,
                               admission: Admission,
                               anchor: Option<&TrustAnchor>,
                               office_head: Option<&str>,
                               old_head: Option<&str>,
                               new_head: &str) -> Result<OfficeState, String> {
    let (Some(anchor), Some(office_head)) = (anchor, office_head) else {
        return Err(format!(
            "@{} needs an office: establish trust and lift the office before syncing it.", pallet
        ));
    };

    let office = audit_utils::verify_office_chain_memoized(anchor, office_head)?;
    verify_meta_content(pallet, rules, &office, admission, old_head, new_head)?;

    Ok(office)
}

/// Rule (b): the parcel's signature verifies against its key in the office, and a revoked key
/// signed it inside the revocation's distrust boundary. Returns the verified key. The caller has
/// already loaded the parcel's body, so only the signature is read here.
fn verify_meta_signature<'o>(pallet: &str,
                             hash: &str,
                             office: &'o OfficeState,
                             distrust_boundaries: &mut DistrustBoundaryMemo) -> Result<&'o KeyRecord, String> {
    match audit_utils::classify_signature_trust(hash, office)? {
        SignatureTrust::Verified { key_id } => {
            Ok(office.find_key(&key_id).expect("classify_signature_trust found this key"))
        }

        SignatureTrust::SignedRevoked { key_id } => {
            let key = office.find_key(&key_id).expect("classify_signature_trust found this key");

            if distrust_boundaries.vouched(key, hash)? {
                return Ok(key);
            }

            if let Some(missing) = distrust_boundaries.unresolved_head(key)? {
                return Err(format!(
                    "Parcel {} on @{} is signed with revoked key {}: this store cannot resolve \
                    the revocation's distrust boundary (boundary parcel {} is not present \
                    locally), so it cannot tell whether this parcel predates the revocation. \
                    Verify against a store with the full history.",
                    hash, pallet, key_id, missing
                ));
            }

            Err(format!(
                "Parcel {} on @{} is signed with key {}, which is revoked, and the parcel is \
                outside the revocation's distrust boundary. The key's holder kept signing after \
                the revocation, or the warehouse has been tampered with.",
                hash, pallet, key_id
            ))
        }

        SignatureTrust::Unsigned => Err(format!(
            "Parcel {} on @{} carries no signature. Every meta parcel must be signed by a key of \
            the office.",
            hash, pallet
        )),

        SignatureTrust::UnknownKey { key_id } => Err(format!(
            "Parcel {} on @{} is signed with key {}, which the office does not track.",
            hash, pallet, key_id
        )),
    }
}

/// The three-tree walk over `cells/` trees: every path both sides changed differently since
/// `base` (`None` is the empty tree). It compares entry hashes only, never loads a cell, and
/// never descends into a subtree both sides agree on. It names conflicts; it resolves none.
pub fn detect_conflicts(base: Option<&str>,
                        ours: Option<&str>,
                        theirs: Option<&str>) -> Result<WalkOutcome, String> {
    detect_conflicts_with(base, ours, theirs, &mut |path, hash| load_level(path, hash, false))
}

fn detect_conflicts_with(base: Option<&str>,
                         ours: Option<&str>,
                         theirs: Option<&str>,
                         load: &mut dyn FnMut(&str, &str) -> Result<Level, String>) -> Result<WalkOutcome, String> {
    let mut outcome = WalkOutcome { decisions: Vec::new(), conflicts: Vec::new() };
    walk_entry("", root_entry(base), root_entry(ours), root_entry(theirs), load, &mut outcome)?;

    Ok(outcome)
}

/// The walk at one position. Rows apply in order; the first match wins.
///
/// | base | ours | theirs | decision |
/// |---|---|---|---|
/// | any | X | X (both absent included) | take, not descended |
/// | B | B | X | take theirs |
/// | B | X | B | take ours |
/// | no present side is a cell | | | recurse |
/// | B | X | Y | conflict |
///
/// The recurse row asks that no *present* side be a cell, rather than that both ours and
/// theirs be subtrees: an absent side (or one emptied to absence) must still be descended into,
/// so that a delete-versus-modify, or a prune-versus-leave, is decided at the exact path and not
/// one level too coarse. It is not a superset of "both are subtrees": a cell in base that both
/// sides turned into different subtrees is a conflict at the cell's path, never a blend of the
/// two subtrees. Do not widen it with an `||`.
fn walk_entry(path: &str,
              base: Entry,
              ours: Entry,
              theirs: Entry,
              load: &mut dyn FnMut(&str, &str) -> Result<Level, String>,
              outcome: &mut WalkOutcome) -> Result<(), String> {
    let base = normalize(base);
    let ours = normalize(ours);
    let theirs = normalize(theirs);

    if ours == theirs {
        outcome.decisions.push((path.to_string(), Decision::TakeIdentical));
        return Ok(());
    }

    if base == ours {
        outcome.decisions.push((path.to_string(), Decision::TakeTheirs));
        return Ok(());
    }

    if base == theirs {
        outcome.decisions.push((path.to_string(), Decision::TakeOurs));
        return Ok(());
    }

    if !base.is_blob() && !ours.is_blob() && !theirs.is_blob() {
        outcome.decisions.push((path.to_string(), Decision::Recurse));

        let base_level = level_of(path, &base, load)?;
        let ours_level = level_of(path, &ours, load)?;
        let theirs_level = level_of(path, &theirs, load)?;

        for name in union_names(&[&base_level, &ours_level, &theirs_level]) {
            walk_entry(
                &join(path, name),
                base_level.get(name).cloned().unwrap_or(Entry::Absent),
                ours_level.get(name).cloned().unwrap_or(Entry::Absent),
                theirs_level.get(name).cloned().unwrap_or(Entry::Absent),
                load,
                outcome,
            )?;
        }

        return Ok(());
    }

    outcome.decisions.push((path.to_string(), Decision::Conflict));
    outcome.conflicts.push(MetaConflict { path: path.to_string(), base, ours, theirs });

    Ok(())
}

/// Plan a replay at the tree level: the local side's net change from `base` to `ours`, as
/// changes to apply on `theirs`. All three are `cells/` trees (`None` is empty).
///
/// A set-then-unset on the local side is no net change, so it can never overwrite a value the
/// remote wrote meanwhile; and every path replayed is one the walk examined. Conflicts refuse
/// the whole replay unless `keep` resolves each of them:
///
/// - `Theirs` drops every local change at or beneath the path, so it also resolves every
///   conflict there. It is accepted on any path with a conflict or a local change at or beneath
///   it — the way to abandon a local edit, or a whole subtree of them.
/// - `Ours` must name a conflicted path exactly, and makes the result there ours's whole value.
///   For a subtree that means re-signing each cell in it that differs from theirs, so one
///   stamped by another operator refuses (`restamp_for_replay`), as any restore of their cell
///   would; `Theirs` is always the way out.
///
/// A `keep` naming a path it cannot apply to is refused, so a stale resolution never passes
/// silently.
pub fn plan_net_changes(base: Option<&str>,
                        ours: Option<&str>,
                        theirs: Option<&str>,
                        keep: &BTreeMap<String, KeepSide>) -> Result<Vec<CellChange>, ReplayError> {
    let walk = detect_conflicts(base, ours, theirs)?;
    let local = net_diff(base, ours)?;

    for (path, side) in keep {
        validate_cell_path(path)?;

        if let Some(other) = keep.keys().find(|other| *other != path && at_or_under(path, other)) {
            return Err(ReplayError::Refused(format!(
                "The resolutions for {} and {} overlap; resolve each path once.", other, path
            )));
        }

        match side {
            KeepSide::Ours if !walk.conflicts.iter().any(|conflict| conflict.path == *path) => {
                return Err(ReplayError::Refused(format!(
                    "{} is not in conflict, so there is no remote value to keep ours over.", path
                )));
            }
            KeepSide::Theirs if !walk.conflicts.iter().any(|conflict| at_or_under(&conflict.path, path))
                && !local.iter().any(|change| at_or_under(&change.path, path)) => {
                return Err(ReplayError::Refused(format!(
                    "{} has no local change and no conflict at or beneath it, so there is nothing \
                    to resolve.",
                    path
                )));
            }
            _ => {}
        }
    }

    let unresolved: Vec<MetaConflict> = walk.conflicts.into_iter()
        .filter(|conflict| !keep.iter().any(|(path, side)| {
            *path == conflict.path || (*side == KeepSide::Theirs && at_or_under(&conflict.path, path))
        }))
        .collect();

    if !unresolved.is_empty() {
        return Err(ReplayError::Conflicts(unresolved));
    }

    let mut changes = Vec::new();

    for change in local {
        if keep.keys().any(|path| at_or_under(&change.path, path)) {
            continue;
        }

        let ours_now = resolve_entry(ours, &change.path)?;
        let theirs_now = resolve_entry(theirs, &change.path)?;

        // Both sides already agree here: nothing to replay.
        if ours_now == theirs_now {
            continue;
        }

        if theirs_now != resolve_entry(base, &change.path)? {
            return Err(ReplayError::Refused(format!(
                "{} changed on both sides, yet the walk named no conflict there. This is a bug; \
                nothing was replayed.",
                change.path
            )));
        }

        changes.push(change);
    }

    for (path, side) in keep {
        if *side == KeepSide::Ours {
            diff_entries(
                path,
                resolve_entry(theirs, path)?,
                resolve_entry(ours, path)?,
                &mut |path, hash| load_level(path, hash, false),
                &mut changes,
            )?;
        }
    }

    Ok(changes)
}

/// Turn planned changes into edits signed by `signer_key_id`, re-stamping each replayed cell.
/// A cell already stamped with that key is replayed as it is. Any other stamp is re-signed only
/// when it resolves, in this office, to the signer's own operator under a key that is active or
/// was revoked for retirement — so a key rotation never wedges a replay. A cell stamped by
/// another operator, or by a key revoked as compromised, cannot honestly be re-signed, and is
/// refused by path.
pub fn restamp_for_replay(changes: &[CellChange],
                          office: &OfficeState,
                          signer_key_id: &str) -> Result<Vec<CellEdit>, String> {
    let current = office.find_key(signer_key_id)
        .ok_or(format!("The signing key {} is not tracked in the office.", signer_key_id))?;

    let mut edits = Vec::new();

    for change in changes {
        let Some(blob) = &change.after else {
            edits.push(CellEdit::Unset { path: change.path.clone() });
            continue;
        };

        let envelope = parse_cell(&object_utils::load_blob(blob)?.content)
            .map_err(|why| format!("The cell {} cannot be replayed: {}.", change.path, why))?;

        if envelope.signer == signer_key_id {
            edits.push(CellEdit::Set { path: change.path.clone(), blob: blob.clone() });
            continue;
        }

        let stamp = office.find_key(&envelope.signer).ok_or(format!(
            "The cell {} is stamped with key {}, which the office does not track, so it cannot be \
            re-signed. Keep theirs at this path to discard the local change.",
            change.path, envelope.signer
        ))?;

        if stamp.operator != current.operator {
            return Err(format!(
                "The cell {} was written by another operator (key {}), so it cannot be re-signed \
                under your key. Keep theirs at this path to discard the local change.",
                change.path, envelope.signer
            ));
        }

        if matches!(stamp.revocation_reason, Some(RevocationReason::Compromise)) {
            return Err(format!(
                "The cell {} is stamped with key {}, which was revoked as compromised, so it \
                cannot be re-signed. Keep theirs at this path to discard the local change.",
                change.path, envelope.signer
            ));
        }

        edits.push(CellEdit::Set {
            path: change.path.clone(),
            blob: store_blob(&encode_cell(signer_key_id, &envelope.schema, &envelope.value))?,
        });
    }

    Ok(edits)
}

/// What a replay of the local net change onto a remote head would apply, planned before
/// anything is signed.
pub struct ReplayPlan {
    theirs: String,
    theirs_cells: Option<String>,
    changes: Vec<CellChange>,
}

impl ReplayPlan {
    /// Whether the remote head already holds everything the replay would apply, so there is
    /// nothing to sign: the local side can simply move to it.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The cells the replay would apply over the remote head.
    pub fn changes(&self) -> &[CellChange] {
        &self.changes
    }
}

/// Plan the replay of the local net change since `base` (the chains' common ancestor, `None`
/// when they share none) onto the remote head `theirs` (see [`plan_net_changes`]). Needs no
/// signing key, so a resolution that drops every local change never needs one.
///
/// # Returns
/// * `Ok(ReplayPlan)` - The changes to apply, possibly none.
/// * `Err(..)`        - Unresolved conflicts, or a refusal.
pub fn plan_replay(pallet: &str,
                   rules: &dyn PalletRules,
                   base: Option<&str>,
                   ours: &str,
                   theirs: &str,
                   keep: &BTreeMap<String, KeepSide>) -> Result<ReplayPlan, ReplayError> {
    let base_cells = match base {
        Some(base) => read_pallet_spine(pallet, base, rules.kind())?.cells,
        None => None,
    };
    let ours_cells = read_pallet_spine(pallet, ours, rules.kind())?.cells;
    let theirs_cells = read_pallet_spine(pallet, theirs, rules.kind())?.cells;

    let changes = plan_net_changes(base_cells.as_deref(), ours_cells.as_deref(), theirs_cells.as_deref(), keep)?;

    Ok(ReplayPlan { theirs: theirs.to_string(), theirs_cells, changes })
}

/// Stack a planned replay onto its remote head as one parcel signed by `signer` whose only
/// parent is that head. The local parcels themselves never leave the clone. The new parcel
/// passes [`verify_meta_content`] against the head before it is returned; no head moves.
///
/// # Returns
/// * `Ok(Some(hash))` - The replayed parcel.
/// * `Ok(None)`       - The remote head already holds the local net change; nothing to replay.
/// * `Err(..)`        - A refusal.
pub fn replay_meta_pallet(pallet: &str,
                          rules: &dyn PalletRules,
                          office: &OfficeState,
                          plan: &ReplayPlan,
                          actor: &Operator,
                          description: &str,
                          signer: &MetaSigner) -> Result<Option<String>, ReplayError> {
    let theirs = plan.theirs.as_str();
    let theirs_cells = &plan.theirs_cells;

    let edits = restamp_for_replay(&plan.changes, office, &signer.key_id)?;
    let new_cells = apply_edits(theirs_cells.as_deref(), &edits)?;

    if new_cells == *theirs_cells {
        return Ok(None);
    }

    let tree = store_meta_tree(rules.kind().format_marker().as_bytes(), new_cells.as_deref())?;
    let parcel = stack_signed_parcel(tree, vec![theirs.to_string()], actor, description, signer)?;

    verify_meta_content(pallet, rules, office, Admission::Admit, Some(theirs), &parcel)?;

    Ok(Some(parcel))
}

/// The newest parcel two linear chains share, or `None` when they share none.
///
/// The two chains are walked a parcel at a time, in turn, so heads that forked recently stop at
/// the fork instead of loading both histories to their roots. What two linear chains share is
/// one suffix, and each walk enters it at the fork, so the first parcel either walk reaches that
/// the other has already passed is the fork itself.
///
/// Linearity is checked only on the parcels walked, down to the fork. Below it, both chains are
/// the history a head was verified with when it was adopted (`verify_meta_content`).
pub fn linear_common_ancestor(a: &str, b: &str) -> Result<Option<String>, String> {
    let mut cursors = [Some(a.to_string()), Some(b.to_string())];
    let mut seen: [HashSet<String>; 2] = [HashSet::new(), HashSet::new()];

    while cursors.iter().any(Option::is_some) {
        for side in 0..2 {
            let Some(hash) = cursors[side].take() else { continue };

            if seen[1 - side].contains(&hash) {
                return Ok(Some(hash));
            }

            let parcel = object_utils::load_parcel(&hash)?;

            if parcel.parents.len() > 1 {
                return Err(format!(
                    "Parcel {} has {} parents, but a meta pallet is linear.", hash, parcel.parents.len()
                ));
            }

            cursors[side] = parcel.parents.first().cloned();
            seen[side].insert(hash);
        }
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    const PALLET: &str = "kv";
    const TEXT: &str = "kv.text 1";

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime.
    struct Scratch {
        _scope: crate::globals::StorageRootScope,
        root: std::path::PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "forklift-meta-tree-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);
            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Kv-shaped rules for the substrate's tests: one known schema, and one pallet rule — a
    /// cell under `locked/` may be set once and never changed or removed.
    struct TestRules;

    impl PalletRules for TestRules {
        fn kind(&self) -> MetaKind {
            MetaKind::Kv
        }

        fn knows_schema(&self, schema: &str) -> bool {
            schema == TEXT
        }

        fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
            match change.changes.iter().find(|c| c.path.starts_with("locked/") && c.before.is_some()) {
                Some(locked) => Err(format!("{} is locked once set", locked.path)),
                None => Ok(()),
            }
        }
    }

    /// An office key: the signing half, and an active record for an operator.
    struct Key {
        signing: SigningKey,
        record: KeyRecord,
    }

    impl Key {
        fn new(operator: &str, seed: u8) -> Key {
            let signing = SigningKey::from_bytes(&[seed; 32]);
            let public = signing.verifying_key();
            let key_id = sign_utils::key_id_for_public_key(public.as_bytes());

            let record = KeyRecord {
                key_id: key_id.clone(),
                operator: operator.to_string(),
                public_key: sign_utils::to_hex(public.as_bytes()),
                issued_at: 1,
                retired_at: None,
                revocation_reason: None,
                distrust_boundary: Vec::new(),
                authorized_by: key_id,
                endorsement: "00".repeat(64),
                proof_of_possession: "00".repeat(64),
            };

            Key { signing, record }
        }

        fn id(&self) -> &str {
            &self.record.key_id
        }

        fn signer(&self) -> MetaSigner<'_> {
            let key_id = self.record.key_id.clone();

            MetaSigner::new(&self.record.key_id, move |hash: &str| Ok(ParcelSignature {
                key_id: key_id.clone(),
                signature: self.signing.sign(hash.as_bytes()).to_bytes().to_vec(),
            }))
        }

        /// This key's record, revoked for `reason` with the given distrust boundary.
        fn revoked(&self, reason: RevocationReason, boundary: Vec<String>) -> KeyRecord {
            let mut record = self.record.clone();
            record.retired_at = Some(2);
            record.revocation_reason = Some(reason);
            record.distrust_boundary = boundary;

            record
        }
    }

    fn office(keys: &[&KeyRecord]) -> OfficeState {
        OfficeState { users: Vec::new(), keys: keys.iter().map(|key| (*key).clone()).collect() }
    }

    fn actor(identifier: &str) -> Operator {
        Operator { name: identifier.to_string(), identifier: identifier.to_string() }
    }

    fn text(content: &str) -> Table {
        let mut table = Table::new();
        table.insert("text", toml_edit::value(content));

        table
    }

    /// Store a `kv.text 1` cell stamped with `key_id`.
    fn cell(key_id: &str, content: &str) -> String {
        store_blob(&encode_cell(key_id, TEXT, &text(content))).unwrap()
    }

    fn set(path: &str, blob: &str) -> CellEdit {
        CellEdit::Set { path: path.to_string(), blob: blob.to_string() }
    }

    fn unset(path: &str) -> CellEdit {
        CellEdit::Unset { path: path.to_string() }
    }

    /// A cells tree holding exactly these path → blob pairs.
    fn cells(entries: &[(&str, &str)]) -> Option<String> {
        let edits: Vec<CellEdit> = entries.iter().map(|(path, blob)| set(path, blob)).collect();

        apply_edits(None, &edits).unwrap()
    }

    /// A signed kv parcel over `cells` with any parents and format marker, built without the
    /// content check and without moving a head — the shape a hostile writer can produce.
    fn forge(parents: &[&str], format: &str, cells: Option<&str>, signer: &MetaSigner) -> String {
        let tree = store_meta_tree(format.as_bytes(), cells).unwrap();

        stack_signed_parcel(
            tree,
            parents.iter().map(|parent| parent.to_string()).collect(),
            &actor("forger"),
            "forged",
            signer,
        ).unwrap()
    }

    fn kv() -> String {
        MetaKind::Kv.format_marker()
    }

    /// The cells tree of a parcel.
    fn cells_of(parcel: &str) -> Option<String> {
        read_pallet_spine(PALLET, parcel, MetaKind::Kv).unwrap().cells
    }

    fn verify(office: &OfficeState, old_head: Option<&str>, new_head: &str) -> Result<(), String> {
        verify_meta_content(PALLET, &TestRules, office, Admission::Admit, old_head, new_head)
    }

    fn write(office: &OfficeState, key: &Key, edits: &[CellEdit]) -> Result<Option<String>, String> {
        stack_meta_edits(PALLET, &TestRules, &VerifiedOffice::unconditioned(office.clone()), edits, &actor(&key.record.operator), "write", &key.signer())
    }

    fn signer_of(read: &CellRead) -> &str {
        match read {
            CellRead::Readable(envelope) => &envelope.signer,
            other => panic!("expected a readable cell, got {:?}", other),
        }
    }

    // --- The three-tree walk, ported case by case from the spike's matrix. These run on real
    // object-store trees; `merge` replays ours's net change onto theirs at the tree level, so
    // what the spike asserted about its merged tree is asserted here about the replay result.

    fn blob(content: &str) -> String {
        store_blob(content.as_bytes()).unwrap()
    }

    fn b(hash: &str) -> Entry {
        Entry::Blob(hash.to_string())
    }

    fn t(hash: &str) -> Entry {
        Entry::Tree(hash.to_string())
    }

    fn old_tracked_spine(tracked: &str) -> String {
        tree(&[("tracked", t(tracked))])
    }

    fn tree(entries: &[(&str, Entry)]) -> String {
        store_level(&entries.iter().map(|(name, entry)| (name.to_string(), entry.clone())).collect()).unwrap()
    }

    fn keep_of(keep: &[(&str, KeepSide)]) -> BTreeMap<String, KeepSide> {
        keep.iter().map(|(path, side)| (path.to_string(), *side)).collect()
    }

    fn merge(base: Option<&str>,
             ours: &str,
             theirs: &str,
             keep: &[(&str, KeepSide)]) -> Result<Option<String>, ReplayError> {
        let changes = plan_net_changes(base, Some(ours), Some(theirs), &keep_of(keep))?;
        let edits: Vec<CellEdit> = changes.into_iter()
            .map(|change| match change.after {
                Some(blob) => CellEdit::Set { path: change.path, blob },
                None => CellEdit::Unset { path: change.path },
            })
            .collect();

        Ok(apply_edits(Some(theirs), &edits)?)
    }

    fn conflict_paths(result: Result<Option<String>, ReplayError>) -> Vec<String> {
        match result {
            Err(ReplayError::Conflicts(conflicts)) => conflicts.into_iter().map(|c| c.path).collect(),
            Err(ReplayError::Refused(why)) => panic!("expected conflicts, got a refusal: {}", why),
            Ok(_) => Vec::new(),
        }
    }

    fn at(root: &Option<String>, path: &str) -> Entry {
        resolve_entry(root.as_deref(), path).unwrap()
    }

    fn decision_at(outcome: &WalkOutcome, path: &str) -> Option<Decision> {
        outcome.decisions.iter().find(|(p, _)| p == path).map(|(_, decision)| *decision)
    }

    /// Case 1: unrelated chains (no base) writing different subjects both survive.
    #[test]
    fn case1_disjoint_no_base_first_sync() {
        let _scratch = Scratch::new("case1");
        let open = blob("open");

        let ours = tree(&[("ab", t(&tree(&[("ab11", t(&tree(&[("status", b(&open))])))])))]);
        let theirs = tree(&[("cd", t(&tree(&[("cd22", t(&tree(&[("status", b(&open))])))])))]);

        let merged = merge(None, &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "ab/ab11/status"), b(&open));
        assert_eq!(at(&merged, "cd/cd22/status"), b(&open));
    }

    /// Case 2: both sides changed a cell to the same value — agreement, not a conflict.
    #[test]
    fn case2_identical_convergence() {
        let _scratch = Scratch::new("case2");
        let (draft, open) = (blob("draft"), blob("open"));

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[("status", b(&open))]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "status"), b(&open));
    }

    /// Case 3: only ours changed — ours is replayed.
    #[test]
    fn case3_one_sided_change() {
        let _scratch = Scratch::new("case3");
        let (draft, open) = (blob("draft"), blob("open"));

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[("status", b(&draft))]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "status"), b(&open));
    }

    /// Case 4: both changed differently — a named conflict, and no value manufactured.
    #[test]
    fn case4_both_changed_different_conflict() {
        let _scratch = Scratch::new("case4");
        let (draft, open, closed) = (blob("draft"), blob("open"), blob("closed"));

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[("status", b(&closed))]);

        assert_eq!(conflict_paths(merge(Some(&base), &ours, &theirs, &[])), vec!["status"]);
    }

    /// Case 5: delete versus modify — a conflict at the cell's own path.
    #[test]
    fn case5_delete_vs_modify_conflict() {
        let _scratch = Scratch::new("case5");
        let (draft, open) = (blob("draft"), blob("open"));

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[]);

        assert_eq!(conflict_paths(merge(Some(&base), &ours, &theirs, &[])), vec!["status"]);
    }

    /// Case 6: a one-sided delete propagates.
    #[test]
    fn case6_one_sided_delete_takes() {
        let _scratch = Scratch::new("case6");
        let draft = blob("draft");

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&draft))]);
        let theirs = tree(&[]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "status"), Entry::Absent);
    }

    /// Case 7: a one-sided type flip (cell to subtree) is taken whole.
    #[test]
    fn case7_one_sided_type_flip_takes() {
        let _scratch = Scratch::new("case7");
        let (open, noted) = (blob("open"), blob("noted"));
        let alice = tree(&[("alice", b(&noted))]);

        let base = tree(&[("status", b(&open))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[("status", t(&alice))]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "status"), t(&alice));
        assert_eq!(at(&merged, "status/alice"), b(&noted));
    }

    /// Case 8: both sides flipped a cell into different subtrees — a conflict at the cell's
    /// path, and keeping ours takes ours's whole subtree, never a blend of the two.
    #[test]
    fn case8_both_sided_type_flip_conflict_then_keep() {
        let _scratch = Scratch::new("case8");
        let (open, a_note, b_note) = (blob("open"), blob("a"), blob("b"));
        let ours_subtree = tree(&[("alice", b(&a_note))]);
        let theirs_subtree = tree(&[("bob", b(&b_note))]);

        let base = tree(&[("status", b(&open))]);
        let ours = tree(&[("status", t(&ours_subtree))]);
        let theirs = tree(&[("status", t(&theirs_subtree))]);

        assert_eq!(conflict_paths(merge(Some(&base), &ours, &theirs, &[])), vec!["status"]);

        let merged = merge(Some(&base), &ours, &theirs, &[("status", KeepSide::Ours)]).unwrap();

        assert_eq!(at(&merged, "status"), t(&ours_subtree));
        assert_eq!(at(&merged, "status/alice"), b(&a_note));
        assert_eq!(at(&merged, "status/bob"), Entry::Absent);
    }

    /// Case 9: a deep subtree both sides agree on is never loaded — asserted on the hashes the
    /// walk asked for, not only on their count.
    #[test]
    fn case9_deep_subtree_short_circuit_never_loads_untouched_sibling() {
        let _scratch = Scratch::new("case9");
        let leaf = blob("unchanged-leaf");
        let mut untouched = tree(&[("leaf", b(&leaf))]);

        for i in 0..4 {
            untouched = tree(&[(&format!("level{}", i), t(&untouched))]);
        }

        let (base_disputed, ours_disputed, theirs_disputed) =
            (blob("base-disputed"), blob("ours-disputed"), blob("theirs-disputed"));

        let base = tree(&[("disputed", b(&base_disputed)), ("untouched", t(&untouched))]);
        let ours = tree(&[("disputed", b(&ours_disputed)), ("untouched", t(&untouched))]);
        let theirs = tree(&[("disputed", b(&theirs_disputed)), ("untouched", t(&untouched))]);

        let mut loaded: Vec<String> = Vec::new();
        let outcome = detect_conflicts_with(Some(&base), Some(&ours), Some(&theirs), &mut |path, hash| {
            loaded.push(hash.to_string());
            load_level(path, hash, false)
        }).unwrap();

        let conflicts: Vec<&str> = outcome.conflicts.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(conflicts, vec!["disputed"]);
        assert_eq!(decision_at(&outcome, "untouched"), Some(Decision::TakeIdentical));
        assert!(!loaded.contains(&untouched), "the untouched subtree must never be loaded");
        assert_eq!(loaded.len(), 3, "exactly the three roots are loaded, got {:?}", loaded);
    }

    /// Case 10: disjoint writes colliding in one fan-out bucket are descended into, not
    /// conflicted at, and both survive.
    #[test]
    fn case10_disjoint_writes_sharing_fanout_bucket() {
        let _scratch = Scratch::new("case10");
        let (old, ours_new, theirs_new) = (blob("unchanged"), blob("ours-new"), blob("theirs-new"));
        let ab00 = tree(&[("status", b(&old))]);

        let base_ab = tree(&[("ab00", t(&ab00))]);
        let ours_ab = tree(&[("ab00", t(&ab00)), ("ab11", t(&tree(&[("status", b(&ours_new))])))]);
        let theirs_ab = tree(&[("ab00", t(&ab00)), ("ab22", t(&tree(&[("status", b(&theirs_new))])))]);

        let base = tree(&[("ab", t(&base_ab))]);
        let ours = tree(&[("ab", t(&ours_ab))]);
        let theirs = tree(&[("ab", t(&theirs_ab))]);

        let outcome = detect_conflicts(Some(&base), Some(&ours), Some(&theirs)).unwrap();
        assert!(outcome.conflicts.is_empty(), "spurious conflict: {:?}", outcome.conflicts);
        assert_eq!(decision_at(&outcome, "ab"), Some(Decision::Recurse));

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "ab/ab00/status"), b(&old));
        assert_eq!(at(&merged, "ab/ab11/status"), b(&ours_new));
        assert_eq!(at(&merged, "ab/ab22/status"), b(&theirs_new));
    }

    /// Case 11: one side pruned an emptied subject, the other left it as an empty tree — the
    /// same value under canonical absence, so no conflict.
    #[test]
    fn case11_prune_vs_leave_after_unset() {
        let _scratch = Scratch::new("case11");
        let x = blob("x");
        let empty = tree(&[]);

        let base_ab = tree(&[("ab00", t(&tree(&[("status", b(&x))])))]);
        let ours_ab = tree(&[]);
        let theirs_ab = tree(&[("ab00", t(&empty))]);
        assert_ne!(ours_ab, theirs_ab);

        let base = tree(&[("ab", t(&base_ab))]);
        let ours = tree(&[("ab", t(&ours_ab))]);
        let theirs = tree(&[("ab", t(&theirs_ab))]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "ab/ab00"), Entry::Absent);
    }

    /// Case 11b: an explicit empty tree on one side, taken by a take row rather than reached
    /// by recursion, must still read as absence.
    #[test]
    fn case11b_explicit_empty_tree_taken_wholesale_must_still_canonicalize() {
        let _scratch = Scratch::new("case11b");
        let (old, new) = (blob("old"), blob("new"));
        let empty = tree(&[]);

        let base_ab = tree(&[("zz00", b(&old))]);
        let ours_ab = tree(&[("zz00", b(&old)), ("ab00", t(&empty))]);
        let theirs_ab = tree(&[("zz00", b(&new))]);

        let base = tree(&[("ab", t(&base_ab))]);
        let ours = tree(&[("ab", t(&ours_ab))]);
        let theirs = tree(&[("ab", t(&theirs_ab))]);

        let outcome = detect_conflicts(Some(&base), Some(&ours), Some(&theirs)).unwrap();
        assert!(outcome.conflicts.is_empty(), "conflicts: {:?}", outcome.conflicts);
        assert_eq!(decision_at(&outcome, "ab"), Some(Decision::Recurse));

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "ab/ab00"), Entry::Absent);
        assert_eq!(at(&merged, "ab/zz00"), b(&new));
    }

    /// Case 12: prune-versus-leave composed with a bucket collision in the same bucket.
    #[test]
    fn case12_prune_vs_leave_composed_with_bucket_collision() {
        let _scratch = Scratch::new("case12");
        let (x, brand_new) = (blob("x"), blob("brand-new"));
        let empty = tree(&[]);

        let base_ab = tree(&[("ab00", t(&tree(&[("status", b(&x))])))]);
        let ours_ab = tree(&[("ab11", t(&tree(&[("status", b(&brand_new))])))]);
        let theirs_ab = tree(&[("ab00", t(&empty))]);

        let base = tree(&[("ab", t(&base_ab))]);
        let ours = tree(&[("ab", t(&ours_ab))]);
        let theirs = tree(&[("ab", t(&theirs_ab))]);

        let merged = merge(Some(&base), &ours, &theirs, &[]).unwrap();

        assert_eq!(at(&merged, "ab/ab00"), Entry::Absent);
        assert_eq!(at(&merged, "ab/ab11/status"), b(&brand_new));
    }

    /// Termination: a resolved conflict completes, and the same inputs without the resolution
    /// refuse again — nothing is consumed by the earlier attempt.
    #[test]
    fn termination_keep_completes_without_keep_refuses_again() {
        let _scratch = Scratch::new("termination");
        let (draft, open, closed) = (blob("draft"), blob("open"), blob("closed"));

        let base = tree(&[("status", b(&draft))]);
        let ours = tree(&[("status", b(&open))]);
        let theirs = tree(&[("status", b(&closed))]);

        assert_eq!(conflict_paths(merge(Some(&base), &ours, &theirs, &[])), vec!["status"]);

        let merged = merge(Some(&base), &ours, &theirs, &[("status", KeepSide::Ours)]).unwrap();
        assert_eq!(at(&merged, "status"), b(&open));

        assert_eq!(conflict_paths(merge(Some(&base), &ours, &theirs, &[])), vec!["status"]);
    }

    // --- The cell envelope and the format marker.

    #[test]
    fn one_signer_writing_one_value_gives_identical_bytes() {
        let signer = "a".repeat(64);

        let first = encode_cell(&signer, TEXT, &text("hello"));
        assert_eq!(first, encode_cell(&signer, TEXT, &text("hello")));
        assert_ne!(first, encode_cell(&"b".repeat(64), TEXT, &text("hello")), "who said it is part of the cell");

        let parsed = parse_cell(&first).unwrap();
        assert_eq!(parsed.signer, signer);
        assert_eq!(parsed.schema, TEXT);
        assert_eq!(parsed.value.get("text").and_then(|item| item.as_str()), Some("hello"));

        // Re-encoding a parsed cell reproduces it, so a re-stamp changes only the signer.
        assert_eq!(encode_cell(&parsed.signer, &parsed.schema, &parsed.value), first);
        assert_eq!(parse_cell(&encode_cell(&signer, TEXT, &Table::new())).unwrap().value.len(), 0);
    }

    #[test]
    fn a_cell_envelope_parses_strictly() {
        let signer = "a".repeat(64);
        let good = format!("signer = \"{}\"\nschema = \"{}\"\n[value]\ntext = \"x\"\n", signer, TEXT);
        assert!(parse_cell(good.as_bytes()).is_ok());

        let bad = [
            ("not toml", "signer = [".to_string()),
            ("no signer", format!("schema = \"{}\"\n[value]\n", TEXT)),
            ("short signer", format!("signer = \"abc\"\nschema = \"{}\"\n[value]\n", TEXT)),
            ("uppercase signer", format!("signer = \"{}\"\nschema = \"{}\"\n[value]\n", "A".repeat(64), TEXT)),
            ("no schema", format!("signer = \"{}\"\n[value]\n", signer)),
            ("empty schema", format!("signer = \"{}\"\nschema = \"\"\n[value]\n", signer)),
            ("no value", format!("signer = \"{}\"\nschema = \"{}\"\n", signer, TEXT)),
            ("scalar value", format!("signer = \"{}\"\nschema = \"{}\"\nvalue = 1\n", signer, TEXT)),
            ("extra field", format!("signer = \"{}\"\nschema = \"{}\"\nat = 1\n[value]\n", signer, TEXT)),
        ];

        for (name, bytes) in bad {
            assert!(parse_cell(bytes.as_bytes()).is_err(), "{} should not parse", name);
        }
    }

    #[test]
    fn the_format_marker_matches_exactly() {
        for kind in MetaKind::ALL {
            assert_eq!(parse_format(kind.format_marker().as_bytes()), FormatRead::Known(kind));
        }

        for unknown in ["forklift-meta 2 kv", "forklift-meta 1 kv\n", "forklift-meta 1 KV", "forklift-meta 1 notes", ""] {
            assert!(matches!(parse_format(unknown.as_bytes()), FormatRead::Unknown(_)), "{:?}", unknown);
        }
    }

    #[test]
    fn path_components_are_restricted() {
        for good in ["ab", "v1.2", "a_b-c", "0x"] {
            assert!(validate_component(good).is_ok(), "{}", good);
        }

        for bad in ["", ".", "..", ".forklift", "-x", "a b", "a/b", "é", "a\\b"] {
            assert!(validate_component(bad).is_err(), "{:?}", bad);
        }

        assert!(validate_cell_path("ab/ab01/status").is_ok());
        assert!(validate_cell_path("ab//status").is_err());
    }

    // --- Writes.

    #[test]
    fn a_write_rebuilds_only_the_changed_spine() {
        let _scratch = Scratch::new("write-spine");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);

        let first = write(&office, &alice, &[
            set("ab/ab01/status", &cell(alice.id(), "one")),
            set("cd/cd01/status", &cell(alice.id(), "two")),
        ]).unwrap().unwrap();

        let second = write(&office, &alice, &[set("ab/ab02/status", &cell(alice.id(), "three"))])
            .unwrap().unwrap();

        let (before, after) = (cells_of(&first), cells_of(&second));
        assert_eq!(at(&before, "cd"), at(&after, "cd"), "the untouched subtree keeps its hash");
        assert_eq!(at(&before, "ab/ab01"), at(&after, "ab/ab01"));
        assert_ne!(at(&before, "ab"), at(&after, "ab"));

        assert_eq!(object_utils::load_parcel(&second).unwrap().parents, vec![first.clone()]);
        assert_eq!(pallet_utils::get_meta_pallet_head(PALLET).unwrap(), Some(second.clone()));

        // A write that changes nothing stacks nothing.
        let same = cell(alice.id(), "three");
        assert_eq!(write(&office, &alice, &[set("ab/ab02/status", &same)]).unwrap(), None);
        assert_eq!(pallet_utils::get_meta_pallet_head(PALLET).unwrap(), Some(second));
    }

    #[test]
    fn a_write_carries_cells_it_cannot_read_through_byte_for_byte() {
        let _scratch = Scratch::new("write-unknown");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);

        let future = store_blob(&encode_cell(alice.id(), "kv.future 9", &text("?"))).unwrap();
        write(&office, &alice, &[set("x", &cell(alice.id(), "1")), set("future", &future)]).unwrap();
        let head = write(&office, &alice, &[set("y", &cell(alice.id(), "2"))]).unwrap().unwrap();

        assert_eq!(at(&cells_of(&head), "future"), b(&future));

        let state = read_meta_head(PALLET, &TestRules).unwrap().unwrap();
        assert!(matches!(&state.cells["future"], CellRead::Unknown { schema, .. } if schema == "kv.future 9"));
        assert_eq!(signer_of(&state.cells["x"]), alice.id());
        assert_eq!(signer_of(&state.cells["y"]), alice.id());
    }

    #[test]
    fn edits_refuse_path_clashes_and_allow_type_flips() {
        let _scratch = Scratch::new("edits");
        let x = blob("x");
        let root = cells(&[("subject/key", &x), ("cell", &x)]);

        assert!(apply_edits(root.as_deref(), &[set("cell/below", &x)]).is_err(), "nothing beneath a cell");
        assert!(apply_edits(root.as_deref(), &[set("subject", &x)]).is_err(), "a subject is not a cell");
        assert!(apply_edits(root.as_deref(), &[unset("subject")]).is_err(), "a subject is not a cell to unset");
        assert!(apply_edits(root.as_deref(), &[set("a", &x), unset("a")]).is_err(), "one edit per path");
        assert!(apply_edits(root.as_deref(), &[set(".hidden", &x)]).is_err(), "a bad component");

        // A cell becomes a subject, and a subject a cell, in one write each.
        let flipped = apply_edits(root.as_deref(), &[unset("cell"), set("cell/below", &x)]).unwrap();
        assert_eq!(at(&flipped, "cell/below"), b(&x));

        let back = apply_edits(flipped.as_deref(), &[unset("cell/below"), set("cell", &x)]).unwrap();
        assert_eq!(back, root);

        // Emptied subtrees are pruned, all the way up.
        assert_eq!(apply_edits(root.as_deref(), &[unset("subject/key"), unset("cell")]).unwrap(), None);
        assert_eq!(apply_edits(root.as_deref(), &[unset("missing/key")]).unwrap(), root, "unsetting nothing is nothing");
        assert_eq!(apply_edits(root.as_deref(), &[unset("cell/below")]).unwrap(), root, "nor is unsetting beneath a cell");
        assert!(apply_edits(root.as_deref(), &[unset("cell/below"), set("cell/other", &x)]).is_err(), "a set beneath it still refuses");
    }

    // --- Reads.

    #[test]
    fn reads_are_tolerant_per_cell() {
        let _scratch = Scratch::new("read-cells");
        let alice = Key::new("alice", 1);
        let signer = alice.signer();

        let malformed = blob("signer = [");
        let unknown = store_blob(&encode_cell(alice.id(), "kv.future 9", &text("?"))).unwrap();
        let root = cells(&[("good", &cell(alice.id(), "ok")), ("broken", &malformed), ("future", &unknown)]);
        let head = forge(&[], &kv(), root.as_deref(), &signer);

        let state = read_meta_state(PALLET, &head, &TestRules).unwrap();

        assert_eq!(state.cells.len(), 3);
        assert_eq!(signer_of(&state.cells["good"]), alice.id());
        assert!(matches!(&state.cells["broken"], CellRead::Malformed(_)));
        assert!(matches!(&state.cells["future"], CellRead::Unknown { schema, .. } if schema == "kv.future 9"));
    }

    #[test]
    fn a_format_this_build_cannot_read_refuses_the_whole_pallet() {
        let _scratch = Scratch::new("read-format");
        let alice = Key::new("alice", 1);
        let signer = alice.signer();
        let root = cells(&[("x", &cell(alice.id(), "1"))]);

        // An old-format pallet: the spine, but no marker — whatever sits beneath it.
        let old_tracked = tree(&[("ab", t(&tree(&[("note", b(&blob("old"))) ])))]);
        let old_tree = tree(&[(".forklift", t(&tree(&[("tracked", t(&old_tracked))])))]);
        let old = stack_signed_parcel(old_tree, Vec::new(), &actor("alice"), "old", &signer).unwrap();
        let error = read_meta_state(PALLET, &old, &TestRules).unwrap_err();
        assert!(error.contains("@kv") && error.contains("older forklift"), "{}", error);

        let newer = forge(&[], "forklift-meta 2 kv", root.as_deref(), &signer);
        let error = read_meta_state(PALLET, &newer, &TestRules).unwrap_err();
        assert!(error.contains("@kv") && error.contains("newer forklift"), "{}", error);

        let tags = forge(&[], &MetaKind::Tags.format_marker(), root.as_deref(), &signer);
        let error = read_meta_state(PALLET, &tags, &TestRules).unwrap_err();
        assert!(error.contains("holds tags metadata"), "{}", error);

        // An older forklift's join parcel had an empty tree.
        let join = stack_signed_parcel(tree(&[]), Vec::new(), &actor("alice"), "join", &signer).unwrap();
        assert!(read_meta_state(PALLET, &join, &TestRules).unwrap_err().contains("older forklift"));

        // Any other shape without a marker is a corrupt spine, not an older forklift's.
        let format_tree = tree(&[("format", t(&tree(&[("x", b(&blob("1")))])))]);
        let corrupt_spines = [
            (tree(&[(".forklift", t(&tree(&[("tracked", t(&format_tree))])))]), "format marker is a subtree"),
            (tree(&[(".forklift", b(&blob("1")))]), "no .forklift/tracked/ spine"),
            (tree(&[(".forklift", t(&tree(&[("tracked", b(&blob("1")))])))]), "no .forklift/tracked/ spine"),
            (tree(&[("src", t(&tree(&[("main.rs", b(&blob("fn main() {}")))])))]), "no .forklift/tracked/ spine"),
            (tree(&[(".forklift", t(&old_tracked_spine(&old_tracked))), ("src", b(&blob("1")))]), "entries besides"),
        ];

        for (corrupt_tree, why) in corrupt_spines {
            let corrupt = stack_signed_parcel(corrupt_tree, Vec::new(), &actor("alice"), "corrupt", &signer).unwrap();
            let error = read_meta_state(PALLET, &corrupt, &TestRules).unwrap_err();
            assert!(error.contains(why) && !error.contains("older forklift"), "{}", error);
        }

        // The writer refuses to stack on any of them, too.
        pallet_utils::update_meta_pallet_head(
            PALLET,
            pallet_utils::get_meta_pallet_head(PALLET).unwrap().as_deref(),
            &old,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        let office = office(&[&alice.record]);
        assert!(write(&office, &alice, &[set("y", &cell(alice.id(), "2"))]).unwrap_err().contains("older forklift"));

        let ok = forge(&[], &kv(), root.as_deref(), &signer);
        assert!(read_meta_state(PALLET, &ok, &TestRules).is_ok());
    }

    // --- The content check, rule by rule, each refusal beside the acceptance it borders.

    #[test]
    fn a_signed_chain_of_own_stamps_verifies() {
        let _scratch = Scratch::new("verify-chain");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);

        let first = write(&office, &alice, &[set("a/x", &cell(alice.id(), "1"))]).unwrap().unwrap();
        let second = write(&office, &bob, &[set("b/y", &cell(bob.id(), "2")), unset("a/x")]).unwrap().unwrap();

        verify(&office, None, &second).unwrap();
        verify(&office, Some(&first), &second).unwrap();
        verify(&office, Some(&second), &second).unwrap();

        let state = read_meta_state(PALLET, &second, &TestRules).unwrap();
        assert_eq!(state.cells.keys().collect::<Vec<_>>(), vec!["b/y"]);
    }

    #[test]
    fn a_forged_stamp_is_refused() {
        let _scratch = Scratch::new("verify-forged-stamp");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let head = write(&office, &alice, &[set("x", &cell(alice.id(), "1"))]).unwrap().unwrap();
        let base = cells_of(&head).unwrap();

        // Bob writes a value and stamps it as Alice's.
        let forged_cells = apply_edits(Some(&base), &[set("z", &cell(alice.id(), "said by alice"))]).unwrap();
        let forged = forge(&[&head], &kv(), forged_cells.as_deref(), &bob.signer());
        let error = verify(&office, Some(&head), &forged).unwrap_err();
        assert!(error.contains(" z ") && error.contains(alice.id()) && error.contains(bob.id()), "{}", error);

        // The same write under his own stamp is fine.
        let own_cells = apply_edits(Some(&base), &[set("z", &cell(bob.id(), "said by bob"))]).unwrap();
        let own = forge(&[&head], &kv(), own_cells.as_deref(), &bob.signer());
        verify(&office, Some(&head), &own).unwrap();
    }

    #[test]
    fn a_signature_naming_the_right_key_with_invalid_bytes_is_refused() {
        let _scratch = Scratch::new("verify-bad-signature");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let root = cells(&[("x", &cell(alice.id(), "1"))]);

        let parcel = forge(&[], &kv(), root.as_deref(), &alice.signer());
        verify(&office, None, &parcel).unwrap();

        // Alice's key id is public; a signature claiming it proves nothing unless it verifies.
        for bytes in [vec![7u8; 64], bob.signing.sign(parcel.as_bytes()).to_bytes().to_vec()] {
            sign_utils::store_parcel_signature(&parcel, &ParcelSignature {
                key_id: alice.id().to_string(),
                signature: bytes,
            }).unwrap();

            let error = verify(&office, None, &parcel).unwrap_err();
            assert!(error.contains("does not verify"), "{}", error);
        }
    }

    #[test]
    fn unsigned_and_unknown_key_parcels_are_refused() {
        let _scratch = Scratch::new("verify-unsigned");
        let (alice, carol) = (Key::new("alice", 1), Key::new("carol", 3));
        let office = office(&[&alice.record]);

        let root = cells(&[("x", &cell(carol.id(), "1"))]);
        let unknown = forge(&[], &kv(), root.as_deref(), &carol.signer());
        let error = verify(&office, None, &unknown).unwrap_err();
        assert!(error.contains("does not track"), "{}", error);

        let tree = store_meta_tree(kv().as_bytes(), root.as_deref()).unwrap();
        let mut object = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree,
            parents: Vec::new(),
            actions: Vec::new(),
            description: None,
        });
        object.store().unwrap();
        let error = verify(&office, None, &object.hash).unwrap_err();
        assert!(error.contains("carries no signature"), "{}", error);
    }

    #[test]
    fn a_revoked_key_signs_only_inside_its_distrust_boundary() {
        let _scratch = Scratch::new("verify-distrust");
        let alice = Key::new("alice", 1);

        let first = forge(&[], &kv(), cells(&[("x", &cell(alice.id(), "1"))]).as_deref(), &alice.signer());
        let after_cells = cells(&[("x", &cell(alice.id(), "1")), ("y", &cell(alice.id(), "2"))]);
        let after = forge(&[&first], &kv(), after_cells.as_deref(), &alice.signer());

        for reason in [RevocationReason::Retirement, RevocationReason::Compromise] {
            let revoked = alice.revoked(reason, vec![first.clone()]);
            let office = office(&[&revoked]);

            verify(&office, None, &first).unwrap();

            let error = verify(&office, Some(&first), &after).unwrap_err();
            assert!(error.contains("outside the revocation's distrust boundary"), "{}", error);
            assert!(verify(&office, None, &after).is_err());
        }

        // A boundary this store cannot resolve says so, rather than alleging anything.
        let never = object_utils::hash_object_bytes(b"a boundary head this store never had");
        let office = office(&[&alice.revoked(RevocationReason::Retirement, vec![never.clone()])]);
        let error = verify(&office, None, &first).unwrap_err();
        assert!(error.contains(&never) && error.contains("cannot resolve"), "{}", error);
    }

    #[test]
    fn a_stamp_copied_to_a_new_path_is_refused() {
        let _scratch = Scratch::new("verify-copy-path");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let said = cell(alice.id(), "said by alice");
        let head = write(&office, &alice, &[set("x", &said)]).unwrap().unwrap();

        let copied = forge(&[&head], &kv(), cells(&[("x", &said), ("copy", &said)]).as_deref(), &bob.signer());
        let error = verify(&office, Some(&head), &copied).unwrap_err();
        assert!(error.contains(" copy ") && error.contains(alice.id()), "{}", error);

        // Leaving Alice's cell where it is, unchanged, while adding his own, is fine.
        let beside = forge(&[&head], &kv(), cells(&[("x", &said), ("mine", &cell(bob.id(), "b"))]).as_deref(), &bob.signer());
        verify(&office, Some(&head), &beside).unwrap();
    }

    #[test]
    fn a_subtree_copied_to_a_new_subject_is_refused() {
        let _scratch = Scratch::new("verify-copy-subject");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let head = write(&office, &alice, &[set("ab/ab01/status", &cell(alice.id(), "approved"))]).unwrap().unwrap();

        // The very subtree hash, verified at ab/ab01, placed again at ab/ab02.
        let subject = at(&cells_of(&head), "ab/ab01");
        let copied = tree(&[("ab", t(&tree(&[("ab01", subject.clone()), ("ab02", subject)])))]);
        let forged = forge(&[&head], &kv(), Some(&copied), &bob.signer());

        let error = verify(&office, Some(&head), &forged).unwrap_err();
        assert!(error.contains("ab/ab02/status"), "{}", error);
    }

    #[test]
    fn a_withdrawn_cell_restored_by_another_signer_is_refused() {
        let _scratch = Scratch::new("verify-restore");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let said = cell(alice.id(), "said by alice");

        write(&office, &alice, &[set("x", &said), set("keep", &cell(alice.id(), "k"))]).unwrap();
        let withdrawn = write(&office, &alice, &[unset("x")]).unwrap().unwrap();
        let restored_cells = apply_edits(cells_of(&withdrawn).as_deref(), &[set("x", &said)]).unwrap();

        let by_bob = forge(&[&withdrawn], &kv(), restored_cells.as_deref(), &bob.signer());
        let error = verify(&office, Some(&withdrawn), &by_bob).unwrap_err();
        assert!(error.contains(" x ") && error.contains(alice.id()), "{}", error);

        let by_alice = forge(&[&withdrawn], &kv(), restored_cells.as_deref(), &alice.signer());
        verify(&office, Some(&withdrawn), &by_alice).unwrap();
    }

    #[test]
    fn a_parcel_with_two_parents_is_refused() {
        let _scratch = Scratch::new("verify-two-parents");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);

        let left = forge(&[], &kv(), cells(&[("x", &cell(alice.id(), "1"))]).as_deref(), &alice.signer());
        let right = forge(&[], &kv(), cells(&[("y", &cell(alice.id(), "2"))]).as_deref(), &alice.signer());
        let both = cells(&[("x", &cell(alice.id(), "1")), ("y", &cell(alice.id(), "2"))]);

        let merged = forge(&[&left, &right], &kv(), both.as_deref(), &alice.signer());
        let error = verify(&office, None, &merged).unwrap_err();
        assert!(error.contains("2 parents"), "{}", error);

        let linear = forge(&[&left], &kv(), both.as_deref(), &alice.signer());
        verify(&office, None, &linear).unwrap();

        // A head that does not descend from the verified one is refused as well.
        let error = verify(&office, Some(&right), &linear).unwrap_err();
        assert!(error.contains("does not descend"), "{}", error);
    }

    #[test]
    fn a_format_change_is_refused() {
        let _scratch = Scratch::new("verify-format");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);
        let root = cells(&[("x", &cell(alice.id(), "1"))]);
        let head = forge(&[], &kv(), root.as_deref(), &alice.signer());

        let tags = forge(&[&head], &MetaKind::Tags.format_marker(), root.as_deref(), &alice.signer());
        let error = verify(&office, Some(&head), &tags).unwrap_err();
        assert!(error.contains("changes the pallet's format marker"), "{}", error);

        let newer = forge(&[], "forklift-meta 2 kv", root.as_deref(), &alice.signer());
        assert!(verify(&office, None, &newer).unwrap_err().contains("newer forklift"));

        let other_kind = forge(&[], &MetaKind::Tags.format_marker(), root.as_deref(), &alice.signer());
        assert!(verify(&office, None, &other_kind).unwrap_err().contains("holds tags metadata"));

        let same = forge(&[&head], &kv(), cells(&[("x", &cell(alice.id(), "2"))]).as_deref(), &alice.signer());
        verify(&office, Some(&head), &same).unwrap();
    }

    #[test]
    fn a_malformed_envelope_is_refused() {
        let _scratch = Scratch::new("verify-malformed");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);
        let good = cell(alice.id(), "1");
        let head = forge(&[], &kv(), cells(&[("x", &good)]).as_deref(), &alice.signer());

        let broken = forge(&[&head], &kv(), cells(&[("x", &good), ("bad", &blob("signer = ["))]).as_deref(), &alice.signer());
        let error = verify(&office, Some(&head), &broken).unwrap_err();
        assert!(error.contains("malformed cell at bad"), "{}", error);

        // An unknown schema is not malformed: a newer forklift may write it, and it is kept.
        let future = store_blob(&encode_cell(alice.id(), "kv.future 9", &text("?"))).unwrap();
        let newer = forge(&[&head], &kv(), cells(&[("x", &good), ("future", &future)]).as_deref(), &alice.signer());
        verify(&office, Some(&head), &newer).unwrap();
    }

    #[test]
    fn a_non_canonical_tree_is_refused() {
        let _scratch = Scratch::new("verify-canonical");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);
        let good = cell(alice.id(), "1");

        let empty_subtree = tree(&[("x", b(&good)), ("ab", t(&tree(&[])))]);
        let parcel = forge(&[], &kv(), Some(&empty_subtree), &alice.signer());
        assert!(verify(&office, None, &parcel).unwrap_err().contains("empty subtree"));

        let empty_cells = forge(&[], &kv(), Some(&tree(&[])), &alice.signer());
        assert!(verify(&office, None, &empty_cells).unwrap_err().contains("must be omitted"));

        let hidden = tree(&[(".hidden", b(&good))]);
        let parcel = forge(&[], &kv(), Some(&hidden), &alice.signer());
        assert!(verify(&office, None, &parcel).unwrap_err().contains(".hidden"));

        let mut executable = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        executable.add_child(TreeItem::new("x".to_string(), good.clone(), DirEntryType::Executable));
        let mut object = LooseObjectBuilder::build_tree(&executable);
        object.store().unwrap();
        let parcel = forge(&[], &kv(), Some(&object.hash), &alice.signer());
        assert!(verify(&office, None, &parcel).unwrap_err().contains("not a cell"));

        let mut both = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        both.add_child(TreeItem::new("x".to_string(), good.clone(), DirEntryType::Normal));
        both.add_child(TreeItem::new("x".to_string(), tree(&[("y", b(&good))]), DirEntryType::Tree));
        let mut object = LooseObjectBuilder::build_tree(&both);
        object.store().unwrap();
        let parcel = forge(&[], &kv(), Some(&object.hash), &alice.signer());
        // The tree decoder itself refuses a name used twice, so the subtree is never shadowed.
        let error = verify(&office, None, &parcel).unwrap_err();
        assert!(error.contains("more than one entry named \"x\""), "{}", error);

        let canonical = forge(&[], &kv(), cells(&[("x", &good)]).as_deref(), &alice.signer());
        verify(&office, None, &canonical).unwrap();
    }

    #[test]
    fn a_pallet_rule_is_enforced_and_a_refused_write_leaves_the_head() {
        let _scratch = Scratch::new("verify-rule");
        let alice = Key::new("alice", 1);
        let office = office(&[&alice.record]);

        let head = write(&office, &alice, &[set("locked/x", &cell(alice.id(), "1"))]).unwrap().unwrap();

        let error = write(&office, &alice, &[set("locked/x", &cell(alice.id(), "2"))]).unwrap_err();
        assert!(error.contains("breaks a rule") && error.contains("locked/x is locked"), "{}", error);
        assert_eq!(pallet_utils::get_meta_pallet_head(PALLET).unwrap(), Some(head.clone()));

        assert!(write(&office, &alice, &[unset("locked/x")]).is_err());
        write(&office, &alice, &[set("open/y", &cell(alice.id(), "3"))]).unwrap().unwrap();
    }

    // --- Replay.

    /// The keys and chains most replay cases share: a root both sides descend from.
    fn shared_root(office: &OfficeState, writer: &Key) -> String {
        write(office, writer, &[set("seed", &cell(writer.id(), "seed"))]).unwrap().unwrap()
    }

    /// Stack a remote parcel on `parent` (built and signed, never the local head).
    fn remote(parent: &str, key: &Key, edits: &[CellEdit]) -> String {
        let cells = apply_edits(cells_of(parent).as_deref(), edits).unwrap();

        forge(&[parent], &kv(), cells.as_deref(), &key.signer())
    }

    fn replay(office: &OfficeState,
              ours: &str,
              theirs: &str,
              keep: &[(&str, KeepSide)],
              signer: &Key) -> Result<Option<String>, ReplayError> {
        let base = linear_common_ancestor(ours, theirs).unwrap();
        let plan = plan_replay(PALLET, &TestRules, base.as_deref(), ours, theirs, &keep_of(keep))?;

        replay_meta_pallet(PALLET, &TestRules, office, &plan, &actor(&signer.record.operator), "replay", &signer.signer())
    }

    fn refusal(result: Result<Option<String>, ReplayError>) -> String {
        match result {
            Err(ReplayError::Refused(why)) => why,
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn a_local_set_then_unset_never_overwrites_a_concurrent_remote_set() {
        let _scratch = Scratch::new("replay-set-unset");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let root = shared_root(&office, &bob);

        let theirs = remote(&root, &bob, &[set("x", &cell(bob.id(), "remote"))]);

        let set_only = write(&office, &alice, &[set("x", &cell(alice.id(), "local"))]).unwrap().unwrap();
        assert_eq!(linear_common_ancestor(&set_only, &theirs).unwrap(), Some(root.clone()));
        assert_eq!(conflict_paths(replay(&office, &set_only, &theirs, &[], &alice)), vec!["x"]);

        let set_then_unset = write(&office, &alice, &[unset("x")]).unwrap().unwrap();
        assert!(replay(&office, &set_then_unset, &theirs, &[], &alice).unwrap().is_none());

        let state = read_meta_state(PALLET, &theirs, &TestRules).unwrap();
        assert_eq!(signer_of(&state.cells["x"]), bob.id());
    }

    #[test]
    fn a_replay_lands_the_local_net_change_on_the_remote_head() {
        let _scratch = Scratch::new("replay-basic");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let root = shared_root(&office, &bob);

        let theirs = remote(&root, &bob, &[set("b", &cell(bob.id(), "remote"))]);
        write(&office, &alice, &[set("a", &cell(alice.id(), "draft"))]).unwrap();
        let ours = write(&office, &alice, &[set("a", &cell(alice.id(), "final")), unset("seed")]).unwrap().unwrap();

        let replayed = replay(&office, &ours, &theirs, &[], &alice).unwrap().unwrap();

        assert_eq!(object_utils::load_parcel(&replayed).unwrap().parents, vec![theirs.clone()]);
        verify(&office, None, &replayed).unwrap();

        let state = read_meta_state(PALLET, &replayed, &TestRules).unwrap();
        assert_eq!(state.cells.keys().collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(at(&cells_of(&replayed), "a"), b(&cell(alice.id(), "final")));
    }

    #[test]
    fn a_replay_across_a_key_rotation_re_stamps_and_a_compromised_stamp_refuses() {
        let _scratch = Scratch::new("replay-rotation");
        let (old, new, bob) = (Key::new("alice", 1), Key::new("alice", 4), Key::new("bob", 2));
        let before = office(&[&old.record, &new.record, &bob.record]);
        let root = shared_root(&before, &bob);

        let theirs = remote(&root, &bob, &[set("y", &cell(bob.id(), "remote"))]);
        let ours = write(&before, &old, &[set("x", &cell(old.id(), "local"))]).unwrap().unwrap();

        let rotated = office(&[&old.revoked(RevocationReason::Retirement, vec![ours.clone()]), &new.record, &bob.record]);
        let replayed = replay(&rotated, &ours, &theirs, &[], &new).unwrap().unwrap();

        let state = read_meta_state(PALLET, &replayed, &TestRules).unwrap();
        assert_eq!(signer_of(&state.cells["x"]), new.id(), "re-stamped under the current key");
        assert_eq!(at(&cells_of(&replayed), "x"), b(&cell(new.id(), "local")));
        assert_eq!(signer_of(&state.cells["y"]), bob.id());

        let compromised = office(&[&old.revoked(RevocationReason::Compromise, vec![ours.clone()]), &new.record, &bob.record]);
        let why = refusal(replay(&compromised, &ours, &theirs, &[], &new));
        assert!(why.contains("The cell x ") && why.contains("compromised"), "{}", why);
    }

    #[test]
    fn another_operators_cell_is_never_re_signed() {
        let _scratch = Scratch::new("replay-other-operator");
        let (alice, bob, carol) = (Key::new("alice", 1), Key::new("bob", 2), Key::new("carol", 3));
        let office = office(&[&alice.record, &bob.record, &carol.record]);
        let root = shared_root(&office, &bob);

        let theirs = remote(&root, &bob, &[set("y", &cell(bob.id(), "remote"))]);

        // Locally, Carol's write arrived from some other remote, then Alice wrote her own.
        let adopted = remote(&root, &carol, &[set("c", &cell(carol.id(), "carol's"))]);
        pallet_utils::update_meta_pallet_head(
            PALLET,
            pallet_utils::get_meta_pallet_head(PALLET).unwrap().as_deref(),
            &adopted,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        let ours = write(&office, &alice, &[set("a", &cell(alice.id(), "alice's"))]).unwrap().unwrap();

        let why = refusal(replay(&office, &ours, &theirs, &[], &alice));
        assert!(why.contains("The cell c ") && why.contains("another operator"), "{}", why);

        // Keeping theirs at that path abandons it, and the rest replays.
        let replayed = replay(&office, &ours, &theirs, &[("c", KeepSide::Theirs)], &alice).unwrap().unwrap();
        let state = read_meta_state(PALLET, &replayed, &TestRules).unwrap();
        assert_eq!(state.cells.keys().collect::<Vec<_>>(), vec!["a", "seed", "y"]);
    }

    #[test]
    fn conflicts_resolve_only_by_an_applicable_keep() {
        let _scratch = Scratch::new("replay-keep");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let root = shared_root(&office, &bob);

        let theirs = remote(&root, &bob, &[set("x", &cell(bob.id(), "remote"))]);
        let ours = write(&office, &alice, &[set("x", &cell(alice.id(), "local")), set("z", &cell(alice.id(), "z"))])
            .unwrap().unwrap();

        match replay(&office, &ours, &theirs, &[], &alice) {
            Err(ReplayError::Conflicts(conflicts)) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].path, "x");
                assert_eq!(conflicts[0].base, Entry::Absent);
                assert_eq!(conflicts[0].ours, b(&cell(alice.id(), "local")));
                assert_eq!(conflicts[0].theirs, b(&cell(bob.id(), "remote")));
            }
            other => panic!("expected a conflict, got {:?}", other.map(|_| ())),
        }

        let kept_ours = replay(&office, &ours, &theirs, &[("x", KeepSide::Ours)], &alice).unwrap().unwrap();
        assert_eq!(at(&cells_of(&kept_ours), "x"), b(&cell(alice.id(), "local")));
        assert_eq!(at(&cells_of(&kept_ours), "z"), b(&cell(alice.id(), "z")));

        let kept_theirs = replay(&office, &ours, &theirs, &[("x", KeepSide::Theirs)], &alice).unwrap().unwrap();
        assert_eq!(at(&cells_of(&kept_theirs), "x"), b(&cell(bob.id(), "remote")));
        assert_eq!(at(&cells_of(&kept_theirs), "z"), b(&cell(alice.id(), "z")));

        // Stale or misplaced resolutions are refused, never ignored.
        let stale = refusal(replay(&office, &ours, &theirs, &[("x", KeepSide::Ours), ("w", KeepSide::Theirs)], &alice));
        assert!(stale.contains("w has no local change and no conflict"), "{}", stale);

        let not_conflicted = refusal(replay(&office, &ours, &theirs, &[("x", KeepSide::Ours), ("z", KeepSide::Ours)], &alice));
        assert!(not_conflicted.contains("z is not in conflict"), "{}", not_conflicted);

        let overlapping = refusal(replay(&office, &ours, &theirs, &[("x", KeepSide::Ours), ("x/deeper", KeepSide::Theirs)], &alice));
        assert!(overlapping.contains("overlap"), "{}", overlapping);
    }

    #[test]
    fn theirs_at_a_subject_resolves_every_conflict_beneath_it() {
        let _scratch = Scratch::new("replay-keep-subtree");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let root = shared_root(&office, &bob);

        let theirs = remote(&root, &bob, &[set("a/x", &cell(bob.id(), "x")), set("a/y", &cell(bob.id(), "y"))]);
        let ours = write(&office, &alice, &[
            set("a/x", &cell(alice.id(), "x local")),
            set("a/y", &cell(alice.id(), "y local")),
            set("z", &cell(alice.id(), "z")),
        ]).unwrap().unwrap();

        assert_eq!(conflict_paths(replay(&office, &ours, &theirs, &[], &alice)), vec!["a/x", "a/y"]);

        let kept = replay(&office, &ours, &theirs, &[("a", KeepSide::Theirs)], &alice).unwrap().unwrap();
        assert_eq!(at(&cells_of(&kept), "a/x"), b(&cell(bob.id(), "x")));
        assert_eq!(at(&cells_of(&kept), "a/y"), b(&cell(bob.id(), "y")));
        assert_eq!(at(&cells_of(&kept), "z"), b(&cell(alice.id(), "z")));

        // Ours resolves only the path it names, so it never covers a subject's conflicts.
        let ours_above = refusal(replay(&office, &ours, &theirs, &[("a", KeepSide::Ours)], &alice));
        assert!(ours_above.contains("a is not in conflict"), "{}", ours_above);
    }

    #[test]
    fn the_common_ancestor_is_the_fork() {
        let _scratch = Scratch::new("ancestor");
        let alice = Key::new("alice", 1);
        let root = forge(&[], &kv(), None, &alice.signer());
        let mut trunk = root.clone();

        for n in 0..3 {
            trunk = remote(&trunk, &alice, &[set("t", &cell(alice.id(), &n.to_string()))]);
        }

        let left = remote(&remote(&trunk, &alice, &[set("l", &cell(alice.id(), "1"))]), &alice, &[set("l", &cell(alice.id(), "2"))]);
        let right = remote(&trunk, &alice, &[set("r", &cell(alice.id(), "1"))]);

        assert_eq!(linear_common_ancestor(&left, &right).unwrap(), Some(trunk.clone()));
        assert_eq!(linear_common_ancestor(&right, &left).unwrap(), Some(trunk.clone()));
        assert_eq!(linear_common_ancestor(&left, &left).unwrap(), Some(left.clone()));
        assert_eq!(linear_common_ancestor(&left, &trunk).unwrap(), Some(trunk.clone()));
        assert_eq!(linear_common_ancestor(&root, &left).unwrap(), Some(root.clone()));

        let unrelated = forge(&[], &kv(), cells(&[("u", &cell(alice.id(), "u"))]).as_deref(), &alice.signer());
        assert_eq!(linear_common_ancestor(&left, &unrelated).unwrap(), None);
    }

    // --- The meta pallets' rules, through the content check that admits their parcels.

    /// An office of these keys, enrolling each key's operator with a role.
    fn office_with_roles(keys: &[(&Key, office_utils::Role)]) -> OfficeState {
        let users = keys.iter().map(|(key, role)| office_utils::UserRecord {
            identifier: key.record.operator.clone(),
            enrolled_at: 1,
            role: *role,
            pallets: Vec::new(),
            identity_root: key.record.key_id.clone(),
            class: office_utils::IdentityClass::Human,
            supervisor: None,
        }).collect();

        OfficeState { users, keys: keys.iter().map(|(key, _)| key.record.clone()).collect() }
    }

    /// Store a `tags.tag 1` cell stamped with `key_id`, pointing at `subject`.
    fn tag_cell(key_id: &str, subject: &str) -> String {
        let mut table = Table::new();
        table.insert("subject", toml_edit::value(subject));
        table.insert("message", toml_edit::value("release"));
        table.insert("tagged_at", toml_edit::value(7));

        store_blob(&encode_cell(key_id, tag_utils::TAG_SCHEMA, &table)).unwrap()
    }

    /// A signed `@tags` parcel on `parents` whose cells are the parent's with `edits` applied.
    fn tags_parcel(parent: Option<&str>, key: &Key, edits: &[CellEdit]) -> String {
        let base = parent.and_then(|parent| read_pallet_spine("tags", parent, MetaKind::Tags).unwrap().cells);
        let cells = apply_edits(base.as_deref(), edits).unwrap();
        let parents: Vec<&str> = parent.into_iter().collect();

        forge(&parents, &MetaKind::Tags.format_marker(), cells.as_deref(), &key.signer())
    }

    fn verify_tags(office: &OfficeState,
                   admission: Admission,
                   old_head: Option<&str>,
                   new_head: &str) -> Result<(), String> {
        verify_meta_content("tags", &tag_utils::TagsRules, office, admission, old_head, new_head)
    }

    #[test]
    fn a_tag_is_admitted_from_an_admin_and_audited_from_anyone_the_office_knows() {
        let _scratch = Scratch::new("tags-admin");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office_with_roles(&[(&alice, office_utils::Role::Admin), (&bob, office_utils::Role::Writer)]);
        let subject = "ab".repeat(32);

        let by_admin = tags_parcel(None, &alice, &[set("v1.0", &tag_cell(alice.id(), &subject))]);
        verify_tags(&office, Admission::Admit, None, &by_admin).unwrap();

        let by_writer = tags_parcel(Some(&by_admin), &bob, &[set("v1.1", &tag_cell(bob.id(), &subject))]);
        let error = verify_tags(&office, Admission::Admit, Some(&by_admin), &by_writer).unwrap_err();
        assert!(error.contains("v1.1") && error.contains("not an admin"), "{}", error);

        // Admitted history is audited without the role rule: a tagger demoted since is no
        // reason to refuse what an admit already took.
        verify_tags(&office, Admission::Audit, Some(&by_admin), &by_writer).unwrap();
    }

    #[test]
    fn a_tag_is_never_changed_or_removed_even_by_its_tagger() {
        let _scratch = Scratch::new("tags-immutable");
        let alice = Key::new("alice", 1);
        let office = office_with_roles(&[(&alice, office_utils::Role::Admin)]);
        let (first, second) = ("ab".repeat(32), "cd".repeat(32));

        let head = tags_parcel(None, &alice, &[set("v1.0", &tag_cell(alice.id(), &first))]);
        verify_tags(&office, Admission::Admit, None, &head).unwrap();

        let added = tags_parcel(Some(&head), &alice, &[set("v2.0", &tag_cell(alice.id(), &second))]);
        verify_tags(&office, Admission::Admit, Some(&head), &added).unwrap();

        for admission in [Admission::Admit, Admission::Audit] {
            let moved = tags_parcel(Some(&head), &alice, &[set("v1.0", &tag_cell(alice.id(), &second))]);
            let error = verify_tags(&office, admission, Some(&head), &moved).unwrap_err();
            assert!(error.contains("v1.0 already exists"), "{}", error);

            let removed = tags_parcel(Some(&head), &alice, &[unset("v1.0")]);
            let error = verify_tags(&office, admission, Some(&head), &removed).unwrap_err();
            assert!(error.contains("v1.0 already exists"), "{}", error);
        }
    }

    #[test]
    fn a_tag_needs_a_valid_name_and_a_strict_value() {
        let _scratch = Scratch::new("tags-shape");
        let alice = Key::new("alice", 1);
        let office = office_with_roles(&[(&alice, office_utils::Role::Admin)]);

        let nested = tags_parcel(None, &alice, &[set("rel/v1", &tag_cell(alice.id(), &"ab".repeat(32)))]);
        assert!(verify_tags(&office, Admission::Audit, None, &nested).unwrap_err().contains("not a valid tag name"));

        let short = tags_parcel(None, &alice, &[set("v1.0", &tag_cell(alice.id(), "abc"))]);
        assert!(verify_tags(&office, Admission::Audit, None, &short).unwrap_err().contains("full parcel hash"));
    }

    /// Store a manifest cell of `kind` with only a body, stamped with `key_id`.
    fn note_cell(key_id: &str, kind: manifest_utils::ManifestKind, body: &str) -> String {
        let mut table = Table::new();
        table.insert("body", toml_edit::value(body));

        store_blob(&encode_cell(key_id, kind.schema(), &table)).unwrap()
    }

    fn note_entry(subject: &str, kind: manifest_utils::ManifestKind, body: &str) -> manifest_utils::ManifestEntry {
        manifest_utils::ManifestEntry {
            subject: subject.to_string(),
            kind,
            recorded_at: None,
            body: body.to_string(),
            provenance: None,
            delivery: None,
        }
    }

    fn manifest_parcel(parent: Option<&str>, key: &Key, edits: &[CellEdit]) -> String {
        let base = parent.and_then(|parent| read_pallet_spine("manifest", parent, MetaKind::Manifest).unwrap().cells);
        let cells = apply_edits(base.as_deref(), edits).unwrap();
        let parents: Vec<&str> = parent.into_iter().collect();

        forge(&parents, &MetaKind::Manifest.format_marker(), cells.as_deref(), &key.signer())
    }

    fn verify_manifest(office: &OfficeState, old_head: Option<&str>, new_head: &str) -> Result<(), String> {
        verify_meta_content("manifest", &manifest_utils::ManifestRules, office, Admission::Admit, old_head, new_head)
    }

    #[test]
    fn a_manifest_entry_sits_only_where_its_signer_and_content_put_it() {
        use manifest_utils::{cell_path, operator_leaf, ManifestKind};

        let _scratch = Scratch::new("manifest-paths");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let subject = "ab".repeat(32);
        let approval = note_entry(&subject, ManifestKind::Approval, "");
        let note = note_entry(&subject, ManifestKind::Note, "lgtm");

        let own = manifest_parcel(None, &alice, &[
            set(&cell_path("alice", &approval), &note_cell(alice.id(), ManifestKind::Approval, "")),
            set(&cell_path("alice", &note), &note_cell(alice.id(), ManifestKind::Note, "lgtm")),
        ]);
        verify_manifest(&office, None, &own).unwrap();
        assert!(cell_path("alice", &approval).ends_with(&operator_leaf("alice")));

        // Bob writing into Alice's directory is refused whatever the cell holds — including a
        // schema this build cannot read, which would otherwise squat the id Alice's note derives.
        let squatted = store_blob(&encode_cell(bob.id(), "manifest.note 2", &text("lgtm"))).unwrap();
        for (path, blob) in [
            (cell_path("alice", &approval), note_cell(bob.id(), ManifestKind::Approval, "")),
            (cell_path("alice", &note), note_cell(bob.id(), ManifestKind::Note, "lgtm")),
            (cell_path("alice", &note), squatted),
        ] {
            let parcel = manifest_parcel(None, &bob, &[set(&path, &blob)]);
            let error = verify_manifest(&office, None, &parcel).unwrap_err();
            assert!(error.contains("another operator's directory"), "{}", error);
        }

        // In his own directory: an approval is the directory itself, and a note sits at the id
        // its content derives.
        let below = format!("{}/{}", cell_path("bob", &approval), "cd".repeat(32));
        let nested = manifest_parcel(None, &bob, &[set(&below, &note_cell(bob.id(), ManifestKind::Approval, ""))]);
        let error = verify_manifest(&office, None, &nested).unwrap_err();
        assert!(error.contains("not at its signer's own path"), "{}", error);

        let other = note_entry(&subject, ManifestKind::Note, "something else");
        let misplaced = manifest_parcel(None, &bob, &[
            set(&cell_path("bob", &other), &note_cell(bob.id(), ManifestKind::Note, "lgtm")),
        ]);
        let error = verify_manifest(&office, None, &misplaced).unwrap_err();
        assert!(error.contains("not at the id its content derives"), "{}", error);

        let misfiled = manifest_parcel(None, &bob, &[
            set(&cell_path("bob", &approval), &note_cell(bob.id(), ManifestKind::Note, "lgtm")),
        ]);
        let error = verify_manifest(&office, None, &misfiled).unwrap_err();
        assert!(error.contains("holds a note entry under the approval directory"), "{}", error);
    }

    #[test]
    fn a_manifest_entry_is_changed_or_removed_only_by_its_operator() {
        use manifest_utils::{cell_path, ManifestKind};

        let _scratch = Scratch::new("manifest-ownership");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let approval = note_entry(&"ab".repeat(32), ManifestKind::Approval, "");
        let path = cell_path("alice", &approval);

        let head = manifest_parcel(None, &alice, &[set(&path, &note_cell(alice.id(), ManifestKind::Approval, ""))]);

        let removed_by_bob = manifest_parcel(Some(&head), &bob, &[unset(&path)]);
        let error = verify_manifest(&office, Some(&head), &removed_by_bob).unwrap_err();
        assert!(error.contains("another operator's directory"), "{}", error);

        let withdrawn = manifest_parcel(Some(&head), &alice, &[unset(&path)]);
        verify_manifest(&office, Some(&head), &withdrawn).unwrap();

        let again = note_cell(alice.id(), ManifestKind::Approval, "again");
        let rewritten = manifest_parcel(Some(&head), &alice, &[set(&path, &again)]);
        verify_manifest(&office, Some(&head), &rewritten).unwrap();
    }

    /// Store a haul cell of `schema` stamped with `key_id`.
    fn haul_cell(key_id: &str, schema: &str, value: &Table) -> String {
        store_blob(&encode_cell(key_id, schema, value)).unwrap()
    }

    fn haul_parcel(parent: Option<&str>, key: &Key, edits: &[CellEdit]) -> String {
        let base = parent.and_then(|parent| read_pallet_spine("haul", parent, MetaKind::Haul).unwrap().cells);
        let cells = apply_edits(base.as_deref(), edits).unwrap();
        let parents: Vec<&str> = parent.into_iter().collect();

        forge(&parents, &MetaKind::Haul.format_marker(), cells.as_deref(), &key.signer())
    }

    fn verify_haul(office: &OfficeState, old_head: Option<&str>, new_head: &str) -> Result<(), String> {
        verify_meta_content("haul", &haul_utils::HaulRules, office, Admission::Admit, old_head, new_head)
    }

    fn proposal(opener: &str) -> haul_utils::Opened {
        haul_utils::Opened {
            source: "feature".to_string(),
            target: "main".to_string(),
            title: "Add it".to_string(),
            description: String::new(),
            opener: opener.to_string(),
            opened_at: 100,
        }
    }

    /// A `state` cell: `status` at `head`, stamped with `key`.
    fn state_cell(key: &Key, status: &haul_utils::HaulStatus, head: &str) -> String {
        haul_cell(key.id(), haul_utils::SCHEMA_STATE, &haul_utils::state_value(status, head))
    }

    /// The edits that open `opened` as `key`, open at a head.
    fn open_edits(key: &Key, opened: &haul_utils::Opened) -> Vec<CellEdit> {
        let dir = haul_utils::haul_dir(&opened.id());

        vec![
            set(&format!("{}/opened", dir), &haul_cell(key.id(), haul_utils::SCHEMA_OPENED, &haul_utils::opened_value(opened))),
            set(&format!("{}/state", dir), &state_cell(key, &haul_utils::HaulStatus::Open, &"ab".repeat(32))),
        ]
    }

    #[test]
    fn a_haul_is_opened_once_in_its_openers_name_with_a_state() {
        use haul_utils::HaulStatus;

        let _scratch = Scratch::new("haul-opened");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office(&[&alice.record, &bob.record]);
        let opened = proposal("alice");
        let dir = haul_utils::haul_dir(&opened.id());
        let state = format!("{}/state", dir);

        let head = haul_parcel(None, &alice, &open_edits(&alice, &opened));
        verify_haul(&office, None, &head).unwrap();

        // Bob cannot open a haul in Alice's name, even at the id it derives.
        let forged = haul_parcel(None, &bob, &open_edits(&bob, &opened));
        let error = verify_haul(&office, None, &forged).unwrap_err();
        assert!(error.contains("names alice as its opener, but is signed by bob"), "{}", error);

        // A proposal sits at the id it derives, and never without a state.
        let mut moved = proposal("alice");
        moved.title = "Something else".to_string();
        let misplaced = haul_parcel(None, &alice, &[
            set(&format!("{}/opened", dir), &haul_cell(alice.id(), haul_utils::SCHEMA_OPENED, &haul_utils::opened_value(&moved))),
            set(&state, &state_cell(&alice, &HaulStatus::Open, &"ab".repeat(32))),
        ]);
        assert!(verify_haul(&office, None, &misplaced).unwrap_err().contains("not at the id its content derives"));

        let stateless = haul_parcel(None, &alice, &open_edits(&alice, &opened)[..1]);
        assert!(verify_haul(&office, None, &stateless).unwrap_err().contains("opened without a state"));

        for born in [HaulStatus::Closed, HaulStatus::Merged("ef".repeat(32))] {
            let mut edits = open_edits(&alice, &opened);
            edits[1] = set(&state, &state_cell(&alice, &born, &"ab".repeat(32)));
            let error = verify_haul(&office, None, &haul_parcel(None, &alice, &edits)).unwrap_err();
            assert!(error.contains("but a haul is opened open"), "{}", error);
        }

        // Once opened, the proposal never changes or goes, and the state never goes.
        let mut retitled = opened.clone();
        retitled.title = "Renamed".to_string();
        let rewritten = haul_parcel(Some(&head), &alice, &[
            set(&format!("{}/opened", dir), &haul_cell(alice.id(), haul_utils::SCHEMA_OPENED, &haul_utils::opened_value(&retitled))),
        ]);
        assert!(verify_haul(&office, Some(&head), &rewritten).unwrap_err().contains("never changed or removed"));

        let withdrawn = haul_parcel(Some(&head), &alice, &[unset(&format!("{}/opened", dir)), unset(&state)]);
        assert!(verify_haul(&office, Some(&head), &withdrawn).unwrap_err().contains("never"));

        let stateless = haul_parcel(Some(&head), &alice, &[unset(&state)]);
        assert!(verify_haul(&office, Some(&head), &stateless).unwrap_err().contains("never removed"));

        // Only the opener moves the head — to any parcel; anyone may close at the same head.
        let moved = haul_parcel(Some(&head), &alice, &[set(&state, &state_cell(&alice, &HaulStatus::Open, &"cd".repeat(32)))]);
        verify_haul(&office, Some(&head), &moved).unwrap();

        let hijacked = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Open, &"cd".repeat(32)))]);
        let error = verify_haul(&office, Some(&head), &hijacked).unwrap_err();
        assert!(error.contains("only the haul's opener, alice, moves it"), "{}", error);

        let closed = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Closed, &"ab".repeat(32)))]);
        verify_haul(&office, Some(&head), &closed).unwrap();

        let closed_elsewhere = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Closed, &"cd".repeat(32)))]);
        assert!(verify_haul(&office, Some(&head), &closed_elsewhere).unwrap_err().contains("only the haul's opener"));

        // The rules read the state, so a state of a schema this build does not hold is refused
        // rather than carried: carried, it might read merged where this build cannot tell.
        let newer = haul_parcel(Some(&head), &alice, &[set(&state, &haul_cell(alice.id(), "haul.state 2", &text("x")))]);
        assert!(verify_haul(&office, Some(&head), &newer).unwrap_err().contains("holds a haul.state 2 value"));

        // No cell exists in a haul directory without its proposal, nor a cell no rule covers.
        let other = haul_utils::haul_dir(&proposal("bob").id());
        let orphan = haul_parcel(Some(&head), &bob, &[
            set(&format!("{}/state", other), &state_cell(&bob, &HaulStatus::Open, &"cd".repeat(32))),
        ]);
        assert!(verify_haul(&office, Some(&head), &orphan).unwrap_err().contains("no proposal"));

        let unknown = haul_parcel(Some(&head), &bob, &[
            set(&format!("{}/label", dir), &haul_cell(bob.id(), "haul.label 1", &text("x"))),
        ]);
        assert!(verify_haul(&office, Some(&head), &unknown).unwrap_err().contains("not a cell this forklift knows"));
    }

    #[test]
    fn a_merged_haul_stays_merged_and_reviews_belong_to_their_writers() {
        use haul_utils::HaulStatus;

        let _scratch = Scratch::new("haul-status");
        let (alice, bob) = (Key::new("alice", 1), Key::new("bob", 2));
        let office = office_with_roles(&[(&alice, office_utils::Role::Writer), (&bob, office_utils::Role::Writer)]);
        let opened = proposal("alice");
        let id = opened.id();
        let dir = haul_utils::haul_dir(&id);
        let state = format!("{}/state", dir);
        let at = "ab".repeat(32);

        let head = haul_parcel(None, &alice, &open_edits(&alice, &opened));

        // Closed and reopened freely, by anyone.
        let closed = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Closed, &at))]);
        verify_haul(&office, Some(&head), &closed).unwrap();
        let reopened = haul_parcel(Some(&closed), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Open, &at))]);
        verify_haul(&office, Some(&closed), &reopened).unwrap();

        // Merged is final: never reopened, closed, moved or removed, even by the opener.
        let merged = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Merged("ef".repeat(32)), &at))]);
        verify_haul(&office, Some(&head), &merged).unwrap();

        for edit in [
            set(&state, &state_cell(&alice, &HaulStatus::Open, &at)),
            set(&state, &state_cell(&alice, &HaulStatus::Closed, &at)),
            set(&state, &state_cell(&alice, &HaulStatus::Merged("ef".repeat(32)), &"cd".repeat(32))),
            unset(&state),
        ] {
            let undone = haul_parcel(Some(&merged), &alice, &[edit]);
            let error = verify_haul(&office, Some(&merged), &undone).unwrap_err();
            assert!(error.contains("a merge is final") || error.contains("never removed"), "{}", error);
        }

        // A merge records the head it merged: no one but the opener records it at another head.
        // That is also what refuses a non-opener's merge replayed over the opener's update, and
        // the refusal says how out: keep theirs, and merge again.
        let elsewhere = haul_parcel(Some(&head), &bob, &[set(&state, &state_cell(&bob, &HaulStatus::Merged("ef".repeat(32)), &"cd".repeat(32)))]);
        let error = verify_haul(&office, Some(&head), &elsewhere).unwrap_err();
        assert!(error.contains("only the haul's opener"), "{}", error);
        assert!(error.contains(&format!("keeps theirs at {}/state and runs again", id)), "{}", error);

        // A review sits under its writer's directory and names a full head.
        let review = |key: &Key| haul_cell(key.id(), haul_utils::SCHEMA_REVIEW,
            &haul_utils::review_value(haul_utils::ReviewVerdict::Approve, "", &at, 1));
        let review_path = |operator: &str| format!("{}/review/{}", dir, manifest_utils::operator_leaf(operator));

        let own = haul_parcel(Some(&head), &bob, &[set(&review_path("bob"), &review(&bob))]);
        verify_haul(&office, Some(&head), &own).unwrap();

        let squatted = haul_parcel(Some(&head), &bob, &[set(&review_path("alice"), &review(&bob))]);
        assert!(verify_haul(&office, Some(&head), &squatted).unwrap_err().contains("another operator's directory"));

        let removed = haul_parcel(Some(&own), &alice, &[unset(&review_path("bob"))]);
        assert!(verify_haul(&office, Some(&own), &removed).unwrap_err().contains("another operator's directory"));

        // A review of a newer schema is carried.
        let newer = haul_parcel(Some(&head), &bob, &[set(&review_path("bob"), &haul_cell(bob.id(), "haul.review 2", &text("x")))]);
        verify_haul(&office, Some(&head), &newer).unwrap();

        // A comment sits at the id its content derives.
        let comment = haul_utils::comment_value("hi", 5);
        let path = haul_utils::comment_path(&id, "bob", &comment);
        let commented = haul_parcel(Some(&head), &bob, &[set(&path, &haul_cell(bob.id(), haul_utils::SCHEMA_COMMENT, &comment))]);
        verify_haul(&office, Some(&head), &commented).unwrap();

        let elsewhere = haul_utils::comment_path(&id, "bob", &haul_utils::comment_value("bye", 5));
        let misplaced = haul_parcel(Some(&head), &bob, &[set(&elsewhere, &haul_cell(bob.id(), haul_utils::SCHEMA_COMMENT, &comment))]);
        assert!(verify_haul(&office, Some(&head), &misplaced).unwrap_err().contains("not at the id its content derives"));
    }

    #[test]
    fn a_merge_is_recorded_only_by_a_writer_of_its_target() {
        use haul_utils::HaulStatus;
        use office_utils::Role;

        let _scratch = Scratch::new("haul-merge-writer");
        let keys: Vec<Key> = ["alice", "bob", "carol", "dave", "erin", "frank"].iter().enumerate()
            .map(|(seed, name)| Key::new(name, seed as u8 + 1))
            .collect();
        let [alice, bob, carol, dave, erin, frank] = &keys[..] else { unreachable!() };

        // Frank's key is in the office, but no user record is.
        let mut office = office_with_roles(&[
            (alice, Role::Admin), (bob, Role::Writer), (carol, Role::Writer), (dave, Role::Writer), (erin, Role::Reader),
        ]);
        office.users[2].pallets = vec!["main".to_string()];
        office.users[3].pallets = vec!["other".to_string()];
        office.keys.push(frank.record.clone());

        let opened = proposal("alice");
        let state = format!("{}/state", haul_utils::haul_dir(&opened.id()));
        let at = "ab".repeat(32);
        let head = haul_parcel(None, alice, &open_edits(alice, &opened));

        let merge_by = |key: &Key| haul_parcel(Some(&head), key, &[
            set(&state, &state_cell(key, &HaulStatus::Merged("ef".repeat(32)), &at)),
        ]);

        // An admin, an unrestricted writer and a writer granted the target record it.
        for key in [alice, bob, carol] {
            verify_haul(&office, Some(&head), &merge_by(key)).unwrap();
        }

        // A writer of other pallets, a reader and an operator with no record do not.
        for (key, why) in [(dave, "who may not write it"), (erin, "who may not write it"), (frank, "who has no user record")] {
            let merged = merge_by(key);
            let error = verify_haul(&office, Some(&head), &merged).unwrap_err();
            assert!(error.contains(&format!("records a merge into main, but is signed by {}, {}", key.record.operator, why)), "{}", error);

            // Adopting what a remote already admitted audits the record, not its writer.
            verify_meta_content("haul", &haul_utils::HaulRules, &office, Admission::Audit, Some(&head), &merged).unwrap();
        }

        // Closing and reopening stay anyone's.
        let closed = haul_parcel(Some(&head), erin, &[set(&state, &state_cell(erin, &HaulStatus::Closed, &at))]);
        verify_haul(&office, Some(&head), &closed).unwrap();
        let reopened = haul_parcel(Some(&closed), frank, &[set(&state, &state_cell(frank, &HaulStatus::Open, &at))]);
        verify_haul(&office, Some(&closed), &reopened).unwrap();
    }

    /// Store a working parcel on `parents`, kept distinct by `tag`. Ancestry never reads the tree.
    fn work_parcel(parents: &[&str], tag: &str) -> String {
        let parcel = crate::model::parcel::Parcel {
            tree_hash: "0".repeat(64),
            parents: parents.iter().map(|parent| parent.to_string()).collect(),
            actions: vec![],
            description: Some(tag.to_string()),
        };
        let mut object = crate::builder::object::loose_object_builder::LooseObjectBuilder::build_parcel(&parcel);
        let hash = object.hash.clone();
        object.store().unwrap();

        hash
    }

    fn set_work_head(pallet: &str, head: &str) {
        let old = pallet_utils::get_pallet_head(pallet).unwrap();
        pallet_utils::update_pallet_head_in(
            pallet_utils::PalletNamespace::User, pallet, old.as_deref(), head, &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
    }

    #[test]
    fn a_merge_record_is_judged_against_what_this_warehouse_holds() {
        use haul_utils::{HaulStatus, MergeProof, Unproven};

        let _scratch = Scratch::new("haul-merge-proof");
        let base = work_parcel(&[], "base");
        let head = work_parcel(&[&base], "feature");
        let merge = work_parcel(&[&base, &head], "merge");

        let haul = |status: HaulStatus| haul_utils::Haul {
            id: proposal("alice").id(),
            opened: proposal("alice"),
            head: head.clone(),
            status,
            reviews: Vec::new(),
            comments: Vec::new(),
            unreadable: Vec::new(),
        };
        let proof = |parcel: &str| haul_utils::merge_proof(&haul(HaulStatus::Merged(parcel.to_string()))).unwrap();

        assert_eq!(haul_utils::merge_proof(&haul(HaulStatus::Open)), None, "only a merge is judged");
        assert_eq!(haul_utils::merge_proof(&haul(HaulStatus::Closed)), None);

        assert_eq!(proof(&merge), MergeProof::Unproven(Unproven::TargetNotHeld));

        set_work_head("main", &base);
        assert_eq!(proof(&merge), MergeProof::Unproven(Unproven::NotOnTarget));

        set_work_head("main", &merge);
        assert_eq!(proof(&merge), MergeProof::Verified);
        assert_eq!((MergeProof::Verified.verdict(), MergeProof::Verified.reason()), ("verified", None));

        assert_eq!(proof(&"ef".repeat(32)), MergeProof::Unproven(Unproven::ParcelNotHeld));

        // A parcel that does not contain the haul's head refutes the record, whether ancestry is
        // answered by a plain walk or from the commit graph.
        let unrelated = work_parcel(&[&base], "unrelated");
        assert_eq!(proof(&unrelated), MergeProof::Refuted);
        crate::util::graph_utils::build_from_heads(&[unrelated.clone(), head.clone(), merge.clone()]).unwrap();
        assert_eq!(proof(&unrelated), MergeProof::Refuted);
        assert_eq!((MergeProof::Refuted.verdict(), MergeProof::Refuted.reason()), ("refuted", Some("head_not_contained")));

        // A parcel whose history is cut short is never refuted.
        let dangling = work_parcel(&[&"12".repeat(32)], "dangling");
        assert_eq!(proof(&dangling), MergeProof::Unproven(Unproven::HistoryUnreadable));
        assert_eq!(MergeProof::Unproven(Unproven::HistoryUnreadable).reason(), Some("history_unreadable"));
    }

    #[test]
    fn a_haul_lift_holds_a_merge_only_until_the_remote_target_holds_its_parcel() {
        use haul_utils::{HaulStatus, HeldMerge};

        let _scratch = Scratch::new("haul-held");
        let alice = Key::new("alice", 1);
        let opened = proposal("alice");
        let id = opened.id();
        let state = format!("{}/state", haul_utils::haul_dir(&id));
        let at = "ab".repeat(32);

        let base = work_parcel(&[], "base");
        let feature = work_parcel(&[&base], "feature");
        let merge = work_parcel(&[&base, &feature], "merge");
        let later = work_parcel(&[&merge], "later");

        let remote = haul_parcel(None, &alice, &open_edits(&alice, &opened));
        let local = haul_parcel(Some(&remote), &alice, &[set(&state, &state_cell(&alice, &HaulStatus::Merged(merge.clone()), &at))]);
        let closed = haul_parcel(Some(&remote), &alice, &[set(&state, &state_cell(&alice, &HaulStatus::Closed, &at))]);

        let held = |local: &str, remote_target: Option<&str>| {
            let target = |pallet: &str| (pallet == "main").then(|| remote_target.map(str::to_string)).flatten();
            haul_utils::held_by_lift(local, Some(&remote), &target).unwrap()
        };
        let waiting = vec![HeldMerge { haul: id.clone(), parcel: merge.clone(), target: "main".to_string() }];

        // Through: waiting would not bring the parcel.
        assert!(held(&local, None).is_empty(), "the target is not held here");
        set_work_head("main", &base);
        assert!(held(&local, None).is_empty(), "the parcel is not on the target here");

        // Held: the parcel is on the target here, and the remote's target is not known to hold it.
        set_work_head("main", &later);
        assert_eq!(held(&local, None), waiting, "the remote has no such pallet");
        assert_eq!(held(&local, Some(&"cd".repeat(32))), waiting, "the remote's head is not held here");
        assert_eq!(held(&local, Some(&base)), waiting, "the remote's head lacks the parcel");

        // Through once the remote's target holds it.
        assert!(held(&local, Some(&merge)).is_empty());
        assert!(held(&local, Some(&later)).is_empty());

        // Only a merge waits.
        assert!(held(&closed, None).is_empty());

        // A remote @haul head not held here is a divergence the lift refuses on its own.
        let target = |_: &str| None;
        assert!(haul_utils::held_by_lift(&local, Some(&"cd".repeat(32)), &target).unwrap().is_empty());
        assert_eq!(haul_utils::held_by_lift(&local, None, &target).unwrap(), waiting, "a remote with no @haul yet");
    }

    #[test]
    fn a_tag_or_entry_this_build_cannot_read_is_listed_beside_the_rest() {
        use manifest_utils::{cell_path, ManifestKind};

        let _scratch = Scratch::new("unreadable-rows");
        let alice = Key::new("alice", 1);
        let office = office_with_roles(&[(&alice, office_utils::Role::Admin)]);
        let subject = "ab".repeat(32);
        let newer = store_blob(&encode_cell(alice.id(), "tags.tag 2", &text("from a newer forklift"))).unwrap();

        stack_meta_edits("tags", &tag_utils::TagsRules, &VerifiedOffice::unconditioned(office.clone()), &[
            set("v1.0", &tag_cell(alice.id(), &subject)),
            set("v2.0", &newer),
        ], &actor("alice"), "tags", &alice.signer()).unwrap().unwrap();

        let read = tag_utils::read_tags().unwrap();
        assert_eq!(read.tags.iter().map(|tag| tag.tag.name.as_str()).collect::<Vec<_>>(), vec!["v1.0"]);
        assert_eq!(read.unreadable.len(), 1);
        assert_eq!(read.unreadable[0].name, "v2.0");
        assert!(read.unreadable[0].reason.contains("tags.tag 2"), "{}", read.unreadable[0].reason);
        assert!(tag_utils::find_tag("v2.0").unwrap_err().contains("cannot be read"));
        assert_eq!(tag_utils::find_tag("v1.0").unwrap().unwrap().tagger, alice.id(), "no office on disk");

        let note = note_entry(&subject, ManifestKind::Note, "lgtm");
        let newer_path = format!("{}/{}/review/{}", &subject[..2], subject, manifest_utils::operator_leaf("alice"));
        let newer = store_blob(&encode_cell(alice.id(), "manifest.review 1", &text("a newer kind"))).unwrap();

        stack_meta_edits("manifest", &manifest_utils::ManifestRules, &VerifiedOffice::unconditioned(office.clone()), &[
            set(&cell_path("alice", &note), &note_cell(alice.id(), ManifestKind::Note, "lgtm")),
            set(&newer_path, &newer),
        ], &actor("alice"), "manifest", &alice.signer()).unwrap().unwrap();

        let read = manifest_utils::read_manifest().unwrap();
        assert_eq!(read.entries.len(), 1);
        assert_eq!(read.entries[0].entry.body, "lgtm");
        assert_eq!(read.unreadable.len(), 1);
        assert_eq!(read.unreadable[0].subject.as_deref(), Some(subject.as_str()));
        assert_eq!(read.unreadable[0].path, newer_path);

        // A haul lists, beside its readable cells, a review of a newer schema and a cell name this
        // build does not know — the latter forged, since no rule here admits it.
        let opened = proposal("alice");
        let id = opened.id();
        let dir = haul_utils::haul_dir(&id);
        let mut edits = open_edits(&alice, &opened);
        edits.push(set(&format!("{}/label", dir), &haul_cell(alice.id(), "haul.label 1", &text("x"))));
        edits.push(set(&format!("{}/review/{}", dir, manifest_utils::operator_leaf("alice")),
                       &haul_cell(alice.id(), "haul.review 2", &text("x"))));
        let forged = haul_parcel(None, &alice, &edits);
        pallet_utils::update_pallet_head_in(
            pallet_utils::PalletNamespace::Meta,
            "haul",
            pallet_utils::get_pallet_head_in(pallet_utils::PalletNamespace::Meta, "haul").unwrap().as_deref(),
            &forged,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let haul = haul_utils::find_haul(&id[..8]).unwrap();
        let mut unreadable: Vec<&str> = haul.unreadable.iter().map(|cell| cell.path.as_str()).collect();
        unreadable.sort();
        assert_eq!(unreadable, vec![
            format!("{}/label", id),
            format!("{}/review/{}", id, manifest_utils::operator_leaf("alice")),
        ]);
        assert!(haul.reviews.is_empty());
        assert!(haul_utils::read_hauls().unwrap().stray.is_empty(), "a placed cell is the haul's, not stray");
    }
}
