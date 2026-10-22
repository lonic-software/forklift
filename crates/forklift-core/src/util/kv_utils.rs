//! Custom metadata: `@kv`, free-form text values keyed by parcel and key.
//!
//! The first pallet on the meta-tree substrate (`meta_tree`). A cell sits at
//! `<s[0..2]>/<s>/<key>` under `cells/`, where `s` is the full hash of the parcel the value is
//! about, and holds a `kv.text 1` value: one `text` field of UTF-8. Any writer may set or unset
//! any cell; who wrote a value is its signer stamp, which the content check keeps honest.

use toml_edit::{value, Item, Table};
use crate::model::operator::Operator;
use crate::util::meta_tree::{
    self, CellEdit, CellEnvelope, CellRead, MetaKind, MetaSigner, PalletRules, ParcelChange,
};
use crate::util::audit_utils::VerifiedOffice;
use crate::util::{object_utils, pallet_utils, sign_utils};

/// The name of the custom metadata pallet, reached as `@kv`.
pub const KV_PALLET_NAME: &str = "kv";

/// The one value schema this build writes and reads.
pub const KV_TEXT_SCHEMA: &str = "kv.text 1";

/// The longest key, in bytes.
pub const MAX_KEY_BYTES: usize = 128;

/// The longest value, in bytes of UTF-8.
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// The value table's one field.
const FIELD_TEXT: &str = "text";

/// The pallet rules of `@kv`: the cell layout and the value shape. Who may write is not a rule
/// here — any writer may set or unset any cell.
pub struct KvRules;

impl PalletRules for KvRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Kv
    }

    fn knows_schema(&self, schema: &str) -> bool {
        schema == KV_TEXT_SCHEMA
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        for cell in change.changes {
            parse_cell_path(&cell.path)?;

            let Some(after) = &cell.after else { continue };
            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;

            if envelope.schema == KV_TEXT_SCHEMA {
                text_of(&envelope.value).map_err(|why| format!("the value at {} {}", cell.path, why))?;
            }
        }

        Ok(())
    }

    /// `<subject>` or `<subject>/<key>`, for a full parcel hash `subject`.
    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        let (subject, key) = match spelled.split_once('/') {
            Some((subject, key)) => (subject, Some(key)),
            None => (spelled, None),
        };

        if !object_utils::is_object_hash(subject) {
            return Err(format!(
                "\"{}\" does not name a subject by its full parcel hash (64 lowercase hex characters).",
                subject
            ));
        }

        match key {
            Some(key) => {
                validate_key(key)?;
                Ok(cell_path(subject, key))
            }
            None => Ok(subject_path(subject)),
        }
    }

    fn display_path(&self, path: &str) -> String {
        match path.split_once('/') {
            Some((_fan, rest)) => rest.to_string(),
            None => path.to_string(),
        }
    }

    fn display_value(&self, envelope: &CellEnvelope) -> String {
        match text_of(&envelope.value) {
            Ok(text) => text,
            Err(why) => format!("(unreadable: the value {})", why),
        }
    }
}

/// Check a key: one metadata path component of at most [`MAX_KEY_BYTES`] bytes.
pub fn validate_key(key: &str) -> Result<(), String> {
    meta_tree::validate_component(key)?;

    if key.len() > MAX_KEY_BYTES {
        return Err(format!(
            "The key \"{}…\" is {} bytes long; a key may be at most {} bytes.",
            key.chars().take(32).collect::<String>(), key.len(), MAX_KEY_BYTES
        ));
    }

    Ok(())
}

/// Check a value: UTF-8 (which a `&str` already is) of at most [`MAX_VALUE_BYTES`] bytes.
pub fn validate_value(text: &str) -> Result<(), String> {
    if text.len() > MAX_VALUE_BYTES {
        return Err(format!(
            "The value is {} bytes long; a value may be at most {} bytes.", text.len(), MAX_VALUE_BYTES
        ));
    }

    Ok(())
}

/// The subject's directory, relative to `cells/`.
pub fn subject_path(subject: &str) -> String {
    format!("{}/{}", &subject[..2], subject)
}

/// The cell path of a key on a subject, relative to `cells/`.
pub fn cell_path(subject: &str, key: &str) -> String {
    format!("{}/{}", subject_path(subject), key)
}

/// Split a cell path into its subject and key, refusing any path that is not exactly
/// `<s[0..2]>/<s>/<key>` for a parcel hash `s` and a valid key.
pub fn parse_cell_path(path: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = path.split('/').collect();

    let [fan, subject, key] = parts.as_slice() else {
        return Err(format!("{} is not a <subject>/<key> cell path", path));
    };

    if !object_utils::is_object_hash(subject) || *fan != &subject[..2] {
        return Err(format!("{} does not sit under the directory of a parcel hash", path));
    }

    validate_key(key)?;

    Ok((subject.to_string(), key.to_string()))
}

/// A `kv.text 1` value table.
pub fn text_value(text: &str) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_TEXT, value(text));

    table
}

/// The text of a `kv.text 1` value table: exactly one `text` string, within the size limit.
fn text_of(table: &Table) -> Result<String, String> {
    if let Some((field, _)) = table.iter().find(|(field, _)| *field != FIELD_TEXT) {
        return Err(format!("has an unexpected field \"{}\"", field));
    }

    let text = match table.get(FIELD_TEXT) {
        Some(Item::Value(item)) => item.as_str().ok_or("has a text field that is not a string".to_string())?,
        Some(_) => return Err("has a text field that is not a string".to_string()),
        None => return Err("has no text field".to_string()),
    };

    if text.len() > MAX_VALUE_BYTES {
        return Err(format!("is {} bytes long, over the {}-byte limit", text.len(), MAX_VALUE_BYTES));
    }

    Ok(text.to_string())
}

/// One value as a reader sees it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KvValue {
    /// A `kv.text 1` value.
    Text(String),

    /// A value whose schema this build does not know — written by a newer forklift.
    Unknown { schema: String },

    /// A value this build cannot read.
    Malformed(String),
}

/// One cell of `@kv`: a key on a subject, its value and who wrote it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KvEntry {
    pub subject: String,
    pub key: String,
    pub value: KvValue,

    /// The key id the cell is stamped with; `None` when the envelope does not parse.
    pub signer: Option<String>,
}

fn read_value(read: CellRead) -> (KvValue, Option<String>) {
    match read {
        CellRead::Readable(envelope) => match text_of(&envelope.value) {
            Ok(text) => (KvValue::Text(text), Some(envelope.signer)),
            Err(why) => (KvValue::Malformed(format!("the value {}", why)), Some(envelope.signer)),
        },
        CellRead::Unknown { signer, schema } => (KvValue::Unknown { schema }, Some(signer)),
        CellRead::Malformed(why) => (KvValue::Malformed(why), None),
    }
}

/// Set a key on a subject, as one signed parcel on the local `@kv` head. The caller holds the
/// warehouse lock and passes the verified office.
///
/// # Returns
/// * `Ok(Some(hash))` - The new head.
/// * `Ok(None)`       - The key already held exactly this value from this signer.
pub fn set(subject: &str,
           key: &str,
           text: &str,
           office: &VerifiedOffice,
           actor: &Operator,
           signer: &MetaSigner) -> Result<Option<String>, String> {
    validate_key(key)?;
    validate_value(text)?;

    let edit = meta_tree::set_cell(&cell_path(subject, key), &signer.key_id, KV_TEXT_SCHEMA, &text_value(text))?;
    let description = format!("kv set {} {}", subject, key);

    meta_tree::stack_meta_edits(KV_PALLET_NAME, &KvRules, office, &[edit], actor, &description, signer)
}

/// Remove a key from a subject, as one signed parcel. A key that holds no value is an error.
pub fn unset(subject: &str,
             key: &str,
             office: &VerifiedOffice,
             actor: &Operator,
             signer: &MetaSigner) -> Result<String, String> {
    validate_key(key)?;

    if get(subject, key)?.is_none() {
        return Err(format!("{} has no value for the key \"{}\".", short(subject), key));
    }

    let edit = CellEdit::Unset { path: cell_path(subject, key) };
    let description = format!("kv unset {} {}", subject, key);

    meta_tree::stack_meta_edits(KV_PALLET_NAME, &KvRules, office, &[edit], actor, &description, signer)?
        .ok_or(format!("{} has no value for the key \"{}\".", short(subject), key))
}

/// The value of a key on a subject at the local head, or `None` when it holds none.
pub fn get(subject: &str, key: &str) -> Result<Option<KvEntry>, String> {
    validate_key(key)?;

    let Some(head) = pallet_utils::get_meta_pallet_head(KV_PALLET_NAME)? else {
        return Ok(None);
    };

    Ok(meta_tree::read_cell(KV_PALLET_NAME, &head, &KvRules, &cell_path(subject, key))?.map(|read| {
        let (value, signer) = read_value(read);

        KvEntry { subject: subject.to_string(), key: key.to_string(), value, signer }
    }))
}

/// Every cell at the local head, or only a subject's, in path order.
pub fn list(subject: Option<&str>) -> Result<Vec<KvEntry>, String> {
    let Some(state) = meta_tree::read_meta_head(KV_PALLET_NAME, &KvRules)? else {
        return Ok(Vec::new());
    };

    let mut entries = Vec::new();

    for (path, read) in state.cells {
        let (cell_subject, key) = parse_cell_path(&path)
            .map_err(|why| format!("The @{} pallet holds a cell outside its layout: {}.", KV_PALLET_NAME, why))?;

        if subject.is_some_and(|subject| subject != cell_subject) {
            continue;
        }

        let (value, signer) = read_value(read);
        entries.push(KvEntry { subject: cell_subject, key, value, signer });
    }

    Ok(entries)
}

/// One change to a key, as [`log`] reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KvChange {
    /// The parcel that made the change.
    pub parcel: String,

    /// The key id that signed the parcel, if it is signed.
    pub signer: Option<String>,

    /// The operator recorded on the parcel.
    pub operator: String,

    /// When the parcel was stacked (Unix seconds).
    pub timestamp: i64,

    /// The value after the change; `None` when the change removed it.
    pub value: Option<KvValue>,
}

/// Every change to a key on a subject from the local head back to the root, newest first.
pub fn log(subject: &str, key: &str) -> Result<Vec<KvChange>, String> {
    validate_key(key)?;

    let Some(head) = pallet_utils::get_meta_pallet_head(KV_PALLET_NAME)? else {
        return Ok(Vec::new());
    };

    let mut changes = Vec::new();

    for entry in meta_tree::cell_history(KV_PALLET_NAME, &head, &KvRules, &cell_path(subject, key))? {
        let parcel = object_utils::load_parcel(&entry.parcel)?;
        let action = parcel.actions.first();

        changes.push(KvChange {
            signer: sign_utils::load_parcel_signature(&entry.parcel)?.map(|signature| signature.key_id),
            operator: action.map(|action| action.operator.identifier.clone()).unwrap_or_default(),
            timestamp: action.map(|action| action.timestamp.timestamp()).unwrap_or_default(),
            value: entry.after.map(|read| read_value(read).0),
            parcel: entry.parcel,
        });
    }

    Ok(changes)
}

/// The first 12 characters of a hash, as messages and listings abbreviate it.
pub fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> String {
        "ab".to_string() + &"c".repeat(62)
    }

    #[test]
    fn a_cell_path_round_trips_and_nothing_else_parses() {
        let path = cell_path(&subject(), "ci");
        assert_eq!(path, format!("ab/{}/ci", subject()));
        assert_eq!(parse_cell_path(&path).unwrap(), (subject(), "ci".to_string()));

        for wrong in [
            format!("{}/ci", subject()),                // no fan
            format!("cd/{}/ci", subject()),             // the wrong fan
            format!("ab/{}/ci/deeper", subject()),      // below a key
            "ab/abc/ci".to_string(),                    // not a parcel hash
            format!("ab/{}/{}", subject(), "k".repeat(MAX_KEY_BYTES + 1)),
        ] {
            assert!(parse_cell_path(&wrong).is_err(), "{} must not parse", wrong);
        }
    }

    #[test]
    fn keys_and_values_are_bounded() {
        assert!(validate_key(&"k".repeat(MAX_KEY_BYTES)).is_ok());
        assert!(validate_key(&"k".repeat(MAX_KEY_BYTES + 1)).is_err());
        assert!(validate_key("a/b").is_err(), "a key is one component");
        assert!(validate_key("").is_err());

        assert!(validate_value(&"v".repeat(MAX_VALUE_BYTES)).is_ok());
        assert!(validate_value(&"v".repeat(MAX_VALUE_BYTES + 1)).is_err());
    }

    #[test]
    fn a_text_value_reads_back_and_a_malformed_one_says_why() {
        assert_eq!(text_of(&text_value("multi\nline")).unwrap(), "multi\nline");

        let mut extra = text_value("x");
        extra.insert("other", value("y"));
        assert!(text_of(&extra).unwrap_err().contains("unexpected field"));

        let mut not_text = Table::new();
        not_text.insert(FIELD_TEXT, value(7));
        assert!(text_of(&not_text).unwrap_err().contains("not a string"));

        assert!(text_of(&Table::new()).unwrap_err().contains("no text field"));
        assert!(text_of(&text_value(&"v".repeat(MAX_VALUE_BYTES + 1))).is_err());
    }

    #[test]
    fn a_resolution_names_a_subject_or_one_of_its_keys() {
        assert_eq!(KvRules.resolution_path(&subject()).unwrap(), format!("ab/{}", subject()));
        assert_eq!(KvRules.resolution_path(&format!("{}/ci", subject())).unwrap(), cell_path(&subject(), "ci"));
        assert!(KvRules.resolution_path("main/ci").is_err(), "a revision is not a subject here");
        assert!(KvRules.resolution_path(&format!("{}/a/b", subject())).is_err());

        assert_eq!(KvRules.display_path(&cell_path(&subject(), "ci")), format!("{}/ci", subject()));
    }
}
