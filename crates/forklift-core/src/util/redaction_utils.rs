//! Redactions (FORK-359): a permanent record that one stored object must be removed from the
//! warehouse, and never stored in it again.
//!
//! A redaction is a cell on a meta pallet (`@redactions`, on the meta-tree substrate,
//! `meta_tree`), so who redacted it is the cell's stamp: a signing key under trust, the declared
//! operator in a warehouse that never enrolled. The cell sits at `cells/<hash>`, where `<hash>` is
//! the full hash of the redacted object (a file's blob, or a chunked file's recipe), and holds a
//! `redactions.redaction 1` value: the reason, when it was recorded and, for a recipe, a digest
//! of its ordered chunk list, so the same file chunked the same way is recognised once the recipe
//! itself is gone.
//!
//! A redaction is never changed or removed, wherever the content check runs, and a signed parcel
//! that adds one must be signed by an Admin of the office as it stands when the parcel is admitted
//! — the convention `@tags` follows, for the same reasons. A hash some meta pallet's history names
//! is refused when the redaction is recorded: a meta pallet's history is never rewritten, so its
//! content could never be removed.

use std::collections::HashSet;
use toml_edit::{value, Table};
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::object_type::ObjectType;
use crate::model::operator::Operator;
use crate::util::meta_tree::{
    self, CellEnvelope, CellRead, MetaKind, MetaWriter, PalletRules, ParcelChange,
};
use crate::util::office_utils::{self, OfficeState, Role};
use crate::util::{file_utils, object_utils, pallet_utils};

/// The name of the redactions meta pallet. Lives in the meta namespace, so it is reached as
/// `@redactions` and reserves no user pallet name (DESIGN.html §3.3).
pub const REDACTIONS_PALLET_NAME: &str = "redactions";

/// The one value schema this build writes and reads.
pub const REDACTION_SCHEMA: &str = "redactions.redaction 1";

/// The longest reason, in bytes.
pub const MAX_REASON_BYTES: usize = 1024;

const FIELD_REASON: &str = "reason";
const FIELD_REDACTED_AT: &str = "redacted_at";
const FIELD_CHUNKS: &str = "chunks";

/// One redaction: the object it removes and why. Carries no redactor field — the redactor is the
/// stamp on its cell.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Redaction {
    /// The full hash of the redacted object. Immutable once recorded.
    pub hash: String,

    /// Why it was redacted (never empty).
    pub reason: String,

    /// When it was recorded. Display metadata only — never a security input.
    pub redacted_at: i64,

    /// For a recipe, the digest of its ordered chunk list ([`chunk_list_digest`]); `None` for a
    /// blob.
    pub chunks: Option<String>,
}

/// A redaction together with its authorship: forge-proof when signed, declared when not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedRedaction {
    pub redaction: Redaction,

    /// The operator whose key the cell is stamped with (the key id when the office does not know
    /// that key), or the operator an unsigned cell declares.
    pub redactor: String,

    /// The key id the cell is stamped with, or `None` when it was written without a key.
    pub signer: Option<String>,
}

/// A redaction cell this build cannot read: a newer schema, or a malformed cell. It is listed,
/// never dropped and never allowed to fail a read of the others.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableRedaction {
    /// The redacted hash — the cell's path.
    pub hash: String,

    /// Why it cannot be read.
    pub reason: String,
}

/// Every redaction at one head.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RedactionsRead {
    /// Every readable redaction, by hash.
    pub redactions: Vec<AttributedRedaction>,

    /// Every redaction cell this build cannot read.
    pub unreadable: Vec<UnreadableRedaction>,
}

/// Validate a redaction's cell path: a full object hash.
pub fn validate_redacted_hash(hash: &str) -> Result<(), String> {
    match object_utils::is_object_hash(hash) {
        true => Ok(()),
        false => Err(format!("\"{}\" is not a full object hash", hash)),
    }
}

/// The digest of a recipe's ordered chunk list: the BLAKE3 hash of its chunk hashes, each
/// followed by a newline. Two recipes share it exactly when they list the same chunks in the
/// same order, which is what stays recognisable once a redacted recipe has been deleted.
pub fn chunk_list_digest(chunks: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();

    for chunk in chunks {
        hasher.update(chunk.as_bytes());
        hasher.update(b"\n");
    }

    hasher.finalize().to_hex().to_string()
}

fn validate_reason(reason: &str) -> Result<(), String> {
    if reason.trim().is_empty() {
        return Err("has an empty reason".to_string());
    }

    if reason.len() > MAX_REASON_BYTES {
        return Err(format!(
            "has a reason of {} bytes; at most {} are allowed",
            reason.len(), MAX_REASON_BYTES
        ));
    }

    Ok(())
}

/// A `redactions.redaction 1` value table.
fn redaction_value(redaction: &Redaction) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_REASON, value(redaction.reason.as_str()));
    table.insert(FIELD_REDACTED_AT, value(redaction.redacted_at));

    if let Some(chunks) = &redaction.chunks {
        table.insert(FIELD_CHUNKS, value(chunks.as_str()));
    }

    table
}

/// Parse a `redactions.redaction 1` value table, strictly: only its fields, a non-empty reason,
/// and a chunk-list digest, when present, that is a full hash.
fn parse_value(hash: &str, table: &Table) -> Result<Redaction, String> {
    meta_tree::value_fields_only(table, &[FIELD_REASON, FIELD_REDACTED_AT, FIELD_CHUNKS])?;

    let reason = meta_tree::value_string(table, FIELD_REASON)?;
    validate_reason(&reason)?;

    let chunks = match table.contains_key(FIELD_CHUNKS) {
        true => Some(meta_tree::value_string(table, FIELD_CHUNKS)?),
        false => None,
    };

    if chunks.as_deref().is_some_and(|chunks| !object_utils::is_object_hash(chunks)) {
        return Err("has a chunk-list digest that is not a full hash".to_string());
    }

    Ok(Redaction {
        hash: hash.to_string(),
        reason,
        redacted_at: meta_tree::value_integer(table, FIELD_REDACTED_AT)?,
        chunks,
    })
}

/// The pallet rules of `@redactions`: one cell per full object hash, a strict value, an Admin to
/// add one (checked when a parcel is admitted, not when admitted history is audited), and
/// immutability.
pub struct RedactionsRules;

impl PalletRules for RedactionsRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Redactions
    }

    fn knows_schema(&self, schema: &str) -> bool {
        schema == REDACTION_SCHEMA
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        for cell in change.changes {
            validate_redacted_hash(&cell.path)?;

            if cell.before.is_some() {
                return Err(format!(
                    "the redaction of {} already exists, and a redaction is never changed or \
                    removed",
                    cell.path
                ));
            }

            let Some(after) = &cell.after else { continue };

            if let Some((office, operator)) = change.role_check() {
                let is_admin = office.find_user(operator)
                    .is_some_and(|user| matches!(user.role, Role::Admin));

                if !is_admin {
                    return Err(format!(
                        "the redaction of {} is signed by {}, who is not an admin",
                        cell.path, operator
                    ));
                }
            }

            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;

            if envelope.schema == REDACTION_SCHEMA {
                parse_value(&cell.path, &envelope.value)
                    .map_err(|why| format!("the redaction of {} {}", cell.path, why))?;
            }
        }

        Ok(())
    }

    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        validate_redacted_hash(spelled)?;

        Ok(spelled.to_string())
    }

    fn display_value(&self, envelope: &CellEnvelope) -> String {
        match meta_tree::value_string(&envelope.value, FIELD_REASON) {
            Ok(reason) => format!("redacted: {}", reason),
            Err(why) => format!("(unreadable: the value {})", why),
        }
    }
}

/// What a redaction removes, as the record step resolved it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedactedKind {
    /// A file's content blob.
    Blob,

    /// A chunked file's recipe, with every chunk it names.
    Recipe,
}

/// Decide what `hash` is from the object in the local store. A tree, a parcel or a chunk is
/// refused: a redaction removes one file's content.
///
/// # Returns
/// * `Ok(Some(kind))` - The object is held, and is a blob or a recipe.
/// * `Ok(None)`       - The object is not in the local store.
/// * `Err(String)`    - The object is some other type, or could not be read.
pub fn held_kind(hash: &str) -> Result<Option<RedactedKind>, String> {
    if !file_utils::does_object_exist(hash)? {
        return Ok(None);
    }

    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
    let (object_type, _) = object_utils::peek_object_header(&bytes)
        .map_err(|e| format!("Object {} cannot be read: {:?}", hash, e))?;

    match object_type {
        ObjectType::Blob => Ok(Some(RedactedKind::Blob)),
        ObjectType::Recipe => Ok(Some(RedactedKind::Recipe)),
        other => Err(format!(
            "Object {} is a {:?}, not a file's content; name the file to redact as \
            <revision>:<path>.",
            hash, other
        )),
    }
}

/// The kind a tree entry's content is redacted as, or `None` for an entry with no content of its
/// own (a subtree).
pub fn entry_kind(entry_type: DirEntryType) -> Option<RedactedKind> {
    match entry_type {
        DirEntryType::Tree => None,
        _ if entry_type.is_chunked() => Some(RedactedKind::Recipe),
        _ => Some(RedactedKind::Blob),
    }
}

/// Every meta pallet whose history names `hash` anywhere in a parcel's tree, by name. Walks every
/// meta pallet's whole chain, each parcel and tree once per pallet. An object the walk needs and
/// the store lacks is an error, never a pass: the walk could not tell whether it names `hash`.
pub fn meta_holders(hash: &str) -> Result<Vec<String>, String> {
    let mut holders = Vec::new();

    for name in pallet_utils::list_meta_pallets()? {
        let Some(head) = pallet_utils::get_meta_pallet_head(&name)? else { continue };

        if chain_names(&name, &head, hash)? {
            holders.push(name);
        }
    }

    Ok(holders)
}

/// Whether any parcel reachable from `head`, the head of meta pallet `pallet`, has a tree naming
/// `hash` at any depth.
pub(crate) fn chain_names(pallet: &str, head: &str, hash: &str) -> Result<bool, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending = vec![(head.to_string(), true)];

    // Each entry is an object hash and whether it is a parcel (else a tree).
    while let Some((object, is_parcel)) = pending.pop() {
        if object == hash {
            return Ok(true);
        }

        if !seen.insert(object.clone()) {
            continue;
        }

        // A meta chain is checked from its root wherever it is adopted, so an object missing
        // here is damage, not a partial fetch; the walk fails rather than pass a hash it could
        // not judge.
        let missing = |error: String| match file_utils::does_object_exist(&object) {
            Ok(false) => format!(
                "Object {} of @{}'s history is not in this store, so whether that history names \
                {} cannot be told, and nothing was recorded. Record the redaction in a fresh \
                clone of this warehouse.",
                object, pallet, hash
            ),
            _ => error,
        };

        if is_parcel {
            let parcel = object_utils::load_parcel(&object).map_err(missing)?;
            pending.extend(parcel.parents.into_iter().map(|parent| (parent, true)));
            pending.push((parcel.tree_hash, false));
            continue;
        }

        let tree = object_utils::load_tree_shared(&object).map_err(missing)?;

        if tree.get_files().any(|(_, file)| file.hash == hash) {
            return Ok(true);
        }

        pending.extend(tree.get_subtrees().map(|(_, subtree)| (subtree.hash.clone(), false)));
    }

    Ok(false)
}

/// Record a redaction, as one cell on the local `@redactions` head stamped by `writer`. The
/// caller holds the warehouse lock, and has resolved the hash and checked it against the meta
/// pallets; the content check refuses a signer who is not an admin, and a hash already redacted
/// (a fresh `redacted_at` always changes the cells, so a repeat reaches that check).
pub fn record_redaction(redaction: &Redaction,
                        writer: &MetaWriter,
                        actor: &Operator) -> Result<String, String> {
    validate_redacted_hash(&redaction.hash)?;
    validate_reason(&redaction.reason).map_err(|why| format!("The redaction {}.", why))?;

    let stamp = writer.stamp(actor);
    let value = redaction_value(redaction);
    let edits = [meta_tree::set_cell(&redaction.hash, &stamp, REDACTION_SCHEMA, &value)?];
    let description = format!("redact {}", redaction.hash);

    let rules = &RedactionsRules;
    meta_tree::stack_meta_edits(REDACTIONS_PALLET_NAME, rules, writer, &edits, actor, &description)?
        .ok_or(format!(
            "The redaction of {} changed nothing, so nothing was recorded.",
            redaction.hash
        ))
}

/// Read every redaction at the local head, attributed through the office (read best-effort, so
/// one whose key the local office does not know is attributed by key id) or by the operator an
/// unsigned cell declares, with every cell this build cannot read. An unborn pallet reads as
/// empty.
pub fn read_redactions() -> Result<RedactionsRead, String> {
    let Some(state) = meta_tree::read_meta_head(REDACTIONS_PALLET_NAME, &RedactionsRules)? else {
        return Ok(RedactionsRead::default());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut read = RedactionsRead::default();

    for (hash, cell) in state.cells {
        let parsed = match cell {
            CellRead::Readable(envelope) => parse_value(&hash, &envelope.value)
                .map(|redaction| (redaction, envelope.stamp))
                .map_err(|why| format!("the value {}", why)),
            CellRead::Unknown { schema, .. } => Err(format!(
                "written with the schema \"{}\", which this forklift does not know",
                schema
            )),
            CellRead::Malformed(why) => Err(why),
        };

        match parsed {
            Ok((redaction, stamp)) => read.redactions.push(AttributedRedaction {
                redaction,
                redactor: stamp.operator_in(&office),
                signer: stamp.key_id().map(str::to_string),
            }),
            Err(reason) => read.unreadable.push(UnreadableRedaction { hash, reason }),
        }
    }

    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redaction() -> Redaction {
        Redaction {
            hash: "a".repeat(64),
            reason: "leaked key".to_string(),
            redacted_at: 7,
            chunks: None,
        }
    }

    #[test]
    fn a_redaction_round_trips_through_its_value() {
        let hash = "a".repeat(64);
        assert_eq!(parse_value(&hash, &redaction_value(&redaction())).unwrap(), redaction());

        let mut chunked = redaction();
        chunked.chunks = Some(chunk_list_digest(&["b".repeat(64), "c".repeat(64)]));
        assert_eq!(parse_value(&hash, &redaction_value(&chunked)).unwrap(), chunked);
    }

    #[test]
    fn a_value_is_parsed_strictly() {
        let hash = "a".repeat(64);

        let mut extra = redaction_value(&redaction());
        extra.insert("redactor", value("mallory"));
        assert!(parse_value(&hash, &extra).unwrap_err().contains("unexpected field \"redactor\""));

        let mut empty = redaction();
        empty.reason = "  ".to_string();
        assert!(parse_value(&hash, &redaction_value(&empty)).unwrap_err().contains("empty reason"));

        let mut long = redaction();
        long.reason = "r".repeat(MAX_REASON_BYTES + 1);
        assert!(parse_value(&hash, &redaction_value(&long)).unwrap_err().contains("at most"));

        let mut short = redaction();
        short.chunks = Some("abc".to_string());
        let error = parse_value(&hash, &redaction_value(&short)).unwrap_err();
        assert!(error.contains("not a full hash"), "{}", error);

        let mut untimed = redaction_value(&redaction());
        untimed.insert(FIELD_REDACTED_AT, value("yesterday"));
        assert!(parse_value(&hash, &untimed).unwrap_err().contains("not an integer"));
    }

    #[test]
    fn the_chunk_list_digest_keeps_order() {
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        let digest = |chunks: &[&String]| {
            chunk_list_digest(&chunks.iter().map(|chunk| chunk.to_string()).collect::<Vec<_>>())
        };

        assert_eq!(digest(&[&b, &c]), digest(&[&b, &c]));
        assert_ne!(digest(&[&b, &c]), digest(&[&c, &b]));
        assert_ne!(digest(&[&b]), digest(&[&b, &b]));
        assert!(object_utils::is_object_hash(&digest(&[&b])));
    }

    #[test]
    fn a_redacted_hash_is_a_full_object_hash() {
        assert!(validate_redacted_hash(&"a".repeat(64)).is_ok());

        for bad in ["", "abc", &"a".repeat(63), &"g".repeat(64), "v1.0"] {
            assert!(validate_redacted_hash(bad).is_err(), "{:?} must be refused", bad);
        }
    }
}
