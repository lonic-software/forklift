use chrono::DateTime;
use serde::Serialize;
use forklift_core::error::{sanitize, sanitize_lines};
use forklift_core::util::kv_utils::{self, KvChange, KvEntry, KvValue};
use forklift_core::util::meta_tree::MetaSigner;
use forklift_core::util::office_utils::Role;
use forklift_core::util::{config_utils, object_utils, pallet_utils};
use crate::commands::lower;
use crate::output::{self, CommandOutput};

/// The parcel a value is about. A full parcel hash names it even when the parcel is not held
/// here, so values about parcels this warehouse never fetched stay readable and removable.
fn subject_of(revision: &str) -> Result<String, String> {
    match pallet_utils::resolve_revision(revision) {
        Ok(subject) => Ok(subject),
        Err(_) if object_utils::is_object_hash(revision) => Ok(revision.to_string()),
        Err(why) => Err(why),
    }
}

/// The verified office, the signing key and the operator id of a writer, refusing a reader.
fn require_writer() -> Result<(forklift_core::util::audit_utils::VerifiedOffice, String, String,
                               forklift_core::model::operator::Operator), String> {
    let actor = config_utils::get_operator()?;
    let (office, signing_key_id) = lower::verified_signing_office(&actor)?;

    let user = office.find_user(&actor.identifier).ok_or(format!(
        "\"{}\" is not enrolled; ask an enrolled operator to admit you.", actor.identifier
    ))?;

    if user.role == Role::Reader {
        return Err(format!(
            "\"{}\" is a reader; only writers and admins may change custom metadata.", user.identifier
        ));
    }

    let actor_id = user.identifier.clone();

    Ok((office, signing_key_id, actor_id, actor))
}

pub(crate) fn set(revision: &str, key: &str, value: &str) -> Result<(), String> {
    // A new value is about a parcel that exists: it must resolve here.
    let subject = pallet_utils::resolve_revision(revision)?;
    let (office, signing_key_id, actor_id, actor) = require_writer()?;

    let head = kv_utils::set(&subject, key, value, &office, &actor, &MetaSigner::local(&signing_key_id))?;

    output::emit(&Changed {
        action: "set",
        subject,
        key: key.to_string(),
        value: Some(value.to_string()),
        operator: actor_id,
        unchanged: head.is_none(),
        kv_head: head.or(pallet_utils::get_meta_pallet_head(kv_utils::KV_PALLET_NAME)?),
    });

    Ok(())
}

pub(crate) fn unset(revision: &str, key: &str) -> Result<(), String> {
    let subject = subject_of(revision)?;
    let (office, signing_key_id, actor_id, actor) = require_writer()?;

    let head = kv_utils::unset(&subject, key, &office, &actor, &MetaSigner::local(&signing_key_id))?;

    output::emit(&Changed {
        action: "unset",
        subject,
        key: key.to_string(),
        value: None,
        operator: actor_id,
        unchanged: false,
        kv_head: Some(head),
    });

    Ok(())
}

pub(crate) fn get(revision: &str, key: &str) -> Result<(), String> {
    let subject = subject_of(revision)?;

    let entry = kv_utils::get(&subject, key)?.ok_or(format!(
        "{} has no value for the key \"{}\".", kv_utils::short(&subject), key
    ))?;

    output::emit(&EntryView::from(&entry));

    Ok(())
}

pub(crate) fn list(revision: Option<&str>) -> Result<(), String> {
    let subject = revision.map(subject_of).transpose()?;
    let entries = kv_utils::list(subject.as_deref())?.iter().map(EntryView::from).collect();

    output::emit(&KvListing { subject, entries });

    Ok(())
}

pub(crate) fn log(revision: &str, key: &str) -> Result<(), String> {
    let subject = subject_of(revision)?;
    let changes = kv_utils::log(&subject, key)?.iter().map(ChangeView::from).collect();

    output::emit(&KvLog { subject, key: key.to_string(), changes });

    Ok(())
}

/// A value as the views show it: the text, or why it cannot be shown.
fn value_fields(value: &KvValue) -> (Option<String>, Option<String>) {
    match value {
        KvValue::Text(text) => (Some(text.clone()), None),
        KvValue::Unknown { schema } => (None, Some(format!(
            "a {} value, written by a newer forklift", schema
        ))),
        KvValue::Malformed(why) => (None, Some(format!("malformed: {}", why))),
    }
}

/// A key set or removed.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Changed {
    /// `set` or `unset`.
    action: &'static str,

    subject: String,
    key: String,

    /// The value set; absent for an unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    operator: String,

    /// Whether the key already held exactly this value from this key, so nothing was written.
    unchanged: bool,

    /// The head of the kv pallet after the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    kv_head: Option<String>,
}

impl CommandOutput for Changed {
    fn render_human(&self) {
        match (self.action, self.unchanged) {
            ("set", true) => println!(
                "{} on parcel {} already holds that value; nothing to change.",
                sanitize(&self.key), self.subject
            ),
            ("set", false) => println!("Set {} on parcel {}.", sanitize(&self.key), self.subject),
            _ => println!("Removed {} from parcel {}.", sanitize(&self.key), self.subject),
        }
    }
}

/// One value, with who wrote it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct EntryView {
    subject: String,
    key: String,

    /// The text; absent when this forklift cannot read the value (see `unreadable`).
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    /// Why the value cannot be read: a newer forklift's schema, or a malformed cell.
    #[serde(skip_serializing_if = "Option::is_none")]
    unreadable: Option<String>,

    /// The key id the value is stamped with.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
}

impl From<&KvEntry> for EntryView {
    fn from(entry: &KvEntry) -> Self {
        let (value, unreadable) = value_fields(&entry.value);

        EntryView {
            subject: entry.subject.clone(),
            key: entry.key.clone(),
            value,
            unreadable,
            signer: entry.signer.clone(),
        }
    }
}

impl EntryView {
    fn render_value(&self) -> String {
        match (&self.value, &self.unreadable) {
            (Some(value), _) => sanitize_lines(value),
            (None, Some(why)) => format!("({})", sanitize(why)),
            (None, None) => String::new(),
        }
    }
}

impl CommandOutput for EntryView {
    fn render_human(&self) {
        println!("{}", self.render_value());
    }
}

/// Every value, or a parcel's.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct KvListing {
    /// The parcel the listing is scoped to (absent when listing every value).
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,

    entries: Vec<EntryView>,
}

impl CommandOutput for KvListing {
    fn render_human(&self) {
        if self.entries.is_empty() {
            println!("No custom metadata.");
            return;
        }

        for entry in &self.entries {
            if self.subject.is_none() {
                println!("{} {} = {}", kv_utils::short(&entry.subject), sanitize(&entry.key), entry.render_value());
            } else {
                println!("{} = {}", sanitize(&entry.key), entry.render_value());
            }
        }
    }
}

/// One change to a key.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ChangeView {
    /// The parcel that made the change.
    parcel: String,

    /// The key id that signed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,

    /// The operator the parcel records.
    operator: String,

    /// When the parcel was stacked (Unix seconds).
    timestamp: i64,

    /// Whether the change removed the value.
    removed: bool,

    /// The value after the change; absent for a removal or an unreadable value.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    /// Why the value after the change cannot be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    unreadable: Option<String>,
}

impl From<&KvChange> for ChangeView {
    fn from(change: &KvChange) -> Self {
        let (value, unreadable) = match &change.value {
            Some(value) => value_fields(value),
            None => (None, None),
        };

        ChangeView {
            parcel: change.parcel.clone(),
            signer: change.signer.clone(),
            operator: change.operator.clone(),
            timestamp: change.timestamp,
            removed: change.value.is_none(),
            value,
            unreadable,
        }
    }
}

/// Every change to a key, newest first.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct KvLog {
    subject: String,
    key: String,
    changes: Vec<ChangeView>,
}

impl CommandOutput for KvLog {
    fn render_human(&self) {
        if self.changes.is_empty() {
            println!("{} on parcel {} has never been set.", sanitize(&self.key), self.subject);
            return;
        }

        for change in &self.changes {
            let when = DateTime::from_timestamp(change.timestamp, 0)
                .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                .unwrap_or_default();
            let what = match (&change.value, &change.unreadable, change.removed) {
                (_, _, true) => "removed".to_string(),
                (Some(value), _, _) => format!("= {}", sanitize_lines(value)),
                (None, Some(why), _) => format!("({})", sanitize(why)),
                (None, None, _) => String::new(),
            };

            println!("{} {} {} {}", kv_utils::short(&change.parcel), when, sanitize(&change.operator), what);
        }
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Changed>(),
        crate::docgen::OutputSchema::of::<EntryView>(),
        crate::docgen::OutputSchema::of::<KvListing>(),
        crate::docgen::OutputSchema::of::<KvLog>(),
    ]
}
