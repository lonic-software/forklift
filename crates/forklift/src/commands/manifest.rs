use chrono::Utc;
use serde::Serialize;
use forklift_core::util::manifest_utils::{self, AttributedEntry, ManifestEntry, ManifestKind, Provenance};
use forklift_core::util::{config_utils, pallet_utils};
use crate::cli::ManifestAction;
use crate::commands::office;
use crate::output::{self, CommandOutput};

/// Handle the manifest command: signed post-metadata attached to parcels.
///
/// * `manifest note <rev> -m …`    - attach a signed note to a parcel.
/// * `manifest approve <rev> [-m]` - record a signed approval of a parcel.
/// * `manifest show <rev>`         - show the entries attached to a parcel.
/// * `manifest`                    - list the whole manifest.
///
/// Authorship is the parcel's signature, so `manifest` needs no author argument — you
/// record as yourself, verifiably.
///
/// # Arguments
/// * `action` - The subcommand (`None` lists the whole manifest).
///
/// # Returns
/// * `Ok(())`      - If the command was handled.
/// * `Err(String)` - If trust is not established, the revision is unknown, or an object
///                   could not be read or signed.
pub fn handle_command(action: Option<ManifestAction>) -> Result<(), String> {
    match action {
        Some(ManifestAction::Note { revision, message }) => {
            record(&revision, ManifestKind::Note, message, None)
        }
        Some(ManifestAction::Approve { revision, message }) => {
            record(&revision, ManifestKind::Approval, message.unwrap_or_default(), None)
        }
        Some(ManifestAction::Provenance { revision, model, tool, session, transcript, message }) => {
            let provenance = Provenance { model, tool, session, transcript };
            record(&revision, ManifestKind::Provenance, message.unwrap_or_default(), Some(provenance))
        }
        Some(ManifestAction::Show { revision }) => show(&revision),
        None => list(),
    }
}

/// Record a signed manifest entry about a parcel.
///
/// # Arguments
/// * `revision`   - The parcel the entry is about (a pallet name or a parcel hash).
/// * `kind`       - The kind of entry.
/// * `body`       - The entry message.
/// * `provenance` - The provenance details (only for a provenance entry).
fn record(revision: &str, kind: ManifestKind, body: String, provenance: Option<Provenance>) -> Result<(), String> {
    // The subject must resolve to an actual parcel; `resolve_revision` verifies that.
    let subject = pallet_utils::resolve_revision(revision)?;

    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, actor_id) = office::require_signing_actor(&actor)?;

    let entry = ManifestEntry {
        subject: subject.clone(),
        kind,
        recorded_at: Utc::now().timestamp(),
        body: body.clone(),
        provenance: provenance.clone(),
        delivery: None,
    };

    let description = format!(
        "Recorded {} of parcel {} by \"{}\".",
        kind.as_str(), subject, actor_id
    );

    let manifest_head = manifest_utils::record_entry(&entry, &actor, description, &signing_key_id)?;

    output::emit("manifest", &Recorded {
        kind: kind.as_str().to_string(),
        subject,
        operator: actor_id,
        body,
        manifest_head,
    });

    Ok(())
}

/// Show the entries attached to a parcel.
///
/// # Arguments
/// * `revision` - The parcel whose manifest to show.
fn show(revision: &str) -> Result<(), String> {
    let subject = pallet_utils::resolve_revision(revision)?;

    let entries = manifest_utils::read_manifest()?
        .iter()
        .filter(|attributed| attributed.entry.subject == subject)
        .map(EntryView::from)
        .collect();

    output::emit("manifest", &ManifestView { subject: Some(subject), entries });

    Ok(())
}

/// List the whole manifest (every entry, about every parcel).
fn list() -> Result<(), String> {
    let entries = manifest_utils::read_manifest()?.iter().map(EntryView::from).collect();

    output::emit("manifest", &ManifestView { subject: None, entries });

    Ok(())
}

/// A newly recorded manifest entry.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Recorded {
    kind: String,
    subject: String,
    operator: String,
    body: String,

    /// The new head of the manifest pallet.
    manifest_head: String,
}

impl CommandOutput for Recorded {
    fn render_human(&self) {
        print!("Recorded {} of parcel {} by \"{}\".", self.kind, self.subject, self.operator);

        if self.body.is_empty() {
            println!();
        } else {
            println!(" \"{}\"", self.body);
        }
    }
}

/// The manifest, or the slice of it about one parcel.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ManifestView {
    /// The parcel the view is scoped to (`null` when listing the whole manifest).
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,

    entries: Vec<EntryView>,
}

/// One manifest entry in a view, with its forge-proof author.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct EntryView {
    kind: String,
    subject: String,
    author: String,
    recorded_at: i64,
    body: String,

    /// Provenance fields, present only on a provenance entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcript: Option<String>,

    /// Delivery fields, present only on a delivery entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trail_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoints: Option<i64>,
}

impl From<&AttributedEntry> for EntryView {
    fn from(attributed: &AttributedEntry) -> Self {
        let provenance = attributed.entry.provenance.as_ref();
        let delivery = attributed.entry.delivery.as_ref();

        EntryView {
            kind: attributed.entry.kind.as_str().to_string(),
            subject: attributed.entry.subject.clone(),
            author: attributed.author.clone(),
            recorded_at: attributed.entry.recorded_at,
            body: attributed.entry.body.clone(),
            model: provenance.map(|p| p.model.clone()),
            tool: provenance.and_then(|p| p.tool.clone()),
            session: provenance.and_then(|p| p.session.clone()),
            transcript: provenance.and_then(|p| p.transcript.clone()),
            source: delivery.map(|d| d.source.clone()),
            trail_head: delivery.map(|d| d.trail_head.clone()),
            checkpoints: delivery.map(|d| d.checkpoints),
        }
    }
}

impl CommandOutput for ManifestView {
    fn render_human(&self) {
        if self.entries.is_empty() {
            match &self.subject {
                Some(subject) => println!("No manifest entries for parcel {}.", subject),
                None => println!("The manifest is empty."),
            }
            return;
        }

        if let Some(subject) = &self.subject {
            println!("parcel {}", subject);
        }

        for entry in &self.entries {
            // When scoped to a subject the parcel is already printed; otherwise show it.
            let where_clause = match self.subject {
                Some(_) => String::new(),
                None => format!("  {}", short(&entry.subject)),
            };

            let body = if entry.body.is_empty() {
                String::new()
            } else {
                format!("  \"{}\"", entry.body)
            };

            println!(
                "  {:<10}{}  {}{}",
                entry.kind, where_clause, entry.author, body
            );

            // Provenance carries the how-it-was-made detail on a second, indented line.
            if let Some(model) = &entry.model {
                let mut detail = format!("model {}", model);

                if let Some(tool) = &entry.tool {
                    detail.push_str(&format!(", tool {}", tool));
                }
                if let Some(session) = &entry.session {
                    detail.push_str(&format!(", session {}", session));
                }
                if let Some(transcript) = &entry.transcript {
                    detail.push_str(&format!(", transcript {}", transcript));
                }

                println!("             {}", detail);
            }

            // Delivery carries the trail reference on a second, indented line.
            if let Some(source) = &entry.source {
                println!(
                    "             {} checkpoint(s) from \"{}\", trail tip {}",
                    entry.checkpoints.unwrap_or(0),
                    source,
                    entry.trail_head.as_deref().map(short).unwrap_or_default()
                );
            }
        }
    }
}

/// A short parcel-hash prefix for display (full hashes stay in `--json`).
fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Recorded", schemars::schema_for!(Recorded)),
        ("ManifestView", schemars::schema_for!(ManifestView)),
    ]
}
