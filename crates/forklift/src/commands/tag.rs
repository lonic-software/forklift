use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::error::sanitize;
use forklift_core::util::kv_utils::short;
use forklift_core::util::meta_tree::MetaSigner;
use forklift_core::util::office_utils::{OfficeState, Role};
use forklift_core::util::tag_utils::{AttributedTag, Tag, UnreadableTag};
use forklift_core::util::{config_utils, file_utils, office_utils, pallet_utils, remote_utils, tag_utils};
use crate::cli::TagAction;
use crate::commands::lower;
use crate::output::{self, CommandOutput};

/// Handle the tag command (§9.4d): signed tags / releases. Without a subcommand, the tags
/// are listed.
///
/// * `tag` / `tag list`            - List every tag.
/// * `tag create <name> <rev> -m`  - Create a signed tag (admin only).
/// * `tag show <name>`             - Show one tag in full.
///
/// # Arguments
/// * `action` - The tag subcommand (`None` lists the tags).
///
/// # Returns
/// * `Ok(())`      - If the command completed.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(action: Option<TagAction>) -> Result<(), String> {
    match action {
        Some(TagAction::Create { name, revision, message }) =>
            create(&name, revision, message),
        Some(TagAction::Show { name }) => show(&name).await,
        Some(TagAction::List) | None => list().await,
    }
}

/// Create a signed tag pointing at a revision. A tag is a release marker, so — the on-brand
/// convention of §9.4d — it must be signed by an *admin* key: an authoritative act,
/// verifiable offline against the office chain. A tag name is immutable; a name already in
/// use is refused. Without a revision, the current pallet's head is tagged.
///
/// Both rules are the pallet's own, checked on every write and by the remote; the checks here
/// only refuse early, with a plainer message.
fn create(name: &str, revision: Option<String>, message: Option<String>) -> Result<(), String> {
    tag_utils::validate_tag_name(name)?;

    let subject = match revision {
        Some(revision) => pallet_utils::resolve_revision(&revision)?,
        None => {
            let pallet = pallet_utils::read_position()?.pallet;

            pallet_utils::get_pallet_head(&pallet)?
                .ok_or(format!("Pallet \"{}\" has nothing stacked yet; there is nothing to tag.", pallet))?
        }
    };

    let operator = config_utils::get_operator()?;
    let (office, signing_key_id) = lower::verified_signing_office(&operator)?;

    let is_admin = office.find_user(&operator.identifier)
        .is_some_and(|user| matches!(user.role, Role::Admin));

    if !is_admin {
        return Err(format!(
            "Only an admin may create a tag (a release is signed by an admin key); \"{}\" is not \
            an admin in this office.",
            operator.identifier
        ));
    }

    if let Some(existing) = tag_utils::find_tag(name)? {
        return Err(format!(
            "Tag \"{}\" already exists (points at {}); tags are immutable.",
            name, short(&existing.tag.subject)
        ));
    }

    let tag = Tag {
        name: name.to_string(),
        subject: subject.clone(),
        message: message.unwrap_or_default(),
        tagged_at: chrono::Utc::now().timestamp(),
    };

    let tags_head = tag_utils::record_tag(&tag, &office, &operator, &MetaSigner::local(&signing_key_id))?;

    output::emit(&Created { name: name.to_string(), subject, tags_head });

    Ok(())
}

/// List every tag, attributed to its (forge-proof) tagger, and every tag cell this forklift
/// cannot read.
///
/// A list is an enumeration, not a claim about one referent — a cloned or sparse store
/// legitimately holds `@tags` without every subject's pallet, so an absent subject degrades
/// the row (`subject_absent`) rather than fails the command. A probe that cannot decide is an
/// ordinary command failure instead (see [`probe_subject`]).
async fn list() -> Result<(), String> {
    let read = tag_utils::read_tags()?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let tags = read.tags.iter()
        .map(|attributed| Ok(TagView::of(attributed, &names, &office, probe_subject(&attributed.tag)?)))
        .collect::<Result<Vec<_>, String>>()?;

    let unreadable = read.unreadable.iter().map(UnreadableView::from).collect();

    output::emit(&TagList { tags, unreadable });

    Ok(())
}

/// Show one tag in full.
///
/// Renders even when the subject is absent — it does not refuse. `@tags` syncs in full while a
/// sparse `--only` clone fetches only the pallets it names, so a healthy clone legitimately
/// holds tags whose subject it never fetched, and every field shown comes from the tag itself,
/// never from the subject object.
async fn show(name: &str) -> Result<(), String> {
    let attributed = tag_utils::find_tag(name)?
        .ok_or(format!("No tag named \"{}\" exists.", name))?;

    let present = probe_subject(&attributed.tag)?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit(&TagView::of(&attributed, &names, &office, present));

    Ok(())
}

/// Whether a tag's subject is in this store. The content check guarantees it is a full parcel
/// hash, so the answer is present or absent — never fetched here, or collected after nothing
/// else referenced it. A probe that cannot decide (an I/O error reading the object path) fails
/// the command, naming the tag, rather than mislabel the row.
fn probe_subject(tag: &Tag) -> Result<bool, String> {
    file_utils::does_object_exist(&tag.subject).map_err(|error| format!(
        "Could not determine whether tag \"{}\"'s subject ({}) is present in this store: {}",
        tag.name, tag.subject, error
    ))
}

/// The result of creating a tag.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Created {
    name: String,
    subject: String,

    /// The new head of the @tags meta pallet.
    tags_head: String,
}

impl CommandOutput for Created {
    fn render_human(&self) {
        println!(
            "Created tag \"{}\" -> {} (signed; @tags head {}).",
            self.name, short(&self.subject), short(&self.tags_head),
        );
    }
}

/// The list of tags.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct TagList {
    tags: Vec<TagView>,

    /// Tags this forklift cannot read — a newer schema, or a malformed cell. Listed, never
    /// dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unreadable: Vec<UnreadableView>,
}

impl CommandOutput for TagList {
    fn render_human(&self) {
        if self.tags.is_empty() && self.unreadable.is_empty() {
            println!("No tags yet. Create one with \"forklift tag create <name> <revision>\".");
            return;
        }

        // A tag name passed the naming rules on the way in, so it needs no sanitizing; the
        // tagger and message are another operator's text and do.
        let width = self.tags.iter().map(|tag| tag.name.len())
            .chain(self.unreadable.iter().map(|tag| tag.name.len()))
            .max()
            .unwrap_or(0);

        for tag in &self.tags {
            let message = tag.message.lines().next().filter(|line| !line.is_empty())
                .map(|line| format!("  {}", sanitize(line)))
                .unwrap_or_default();

            // Ahead of the tagger and message, so neither can forge or blank it.
            let absent_marker = if tag.subject_absent { "(subject not in this store)  " } else { "" };

            println!(
                "{:<width$}  {}  {}by {}{}",
                tag.name, short(&tag.subject), absent_marker, tag.tagger_label(), message, width = width,
            );
        }

        for tag in &self.unreadable {
            println!("{:<width$}  (unreadable: {})", tag.name, sanitize(&tag.reason), width = width);
        }
    }
}

/// A tag this forklift cannot read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct UnreadableView {
    name: String,

    /// Why it cannot be read.
    reason: String,
}

impl From<&UnreadableTag> for UnreadableView {
    fn from(tag: &UnreadableTag) -> Self {
        UnreadableView { name: tag.name.clone(), reason: tag.reason.clone() }
    }
}

/// One tag, with its tagger resolved to signed identity metadata.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct TagView {
    name: String,

    /// The parcel the tag points at.
    subject: String,

    /// The tag message (may be empty).
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,

    /// The tagger's pseudonymous operator id (the chain's record), or the key id when the
    /// office does not know the key.
    tagger: String,

    /// The resolved display name, when a resolution hook supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    tagger_name: Option<String>,

    /// The tagger's role in the office, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    tagger_role: Option<String>,

    /// The key id the tag is signed with.
    signer: String,

    /// The tag creation time as RFC 3339 (UTC).
    tagged_at: String,

    /// Present and `true` when the subject parcel is not in this store (never fetched, or
    /// collected); omitted when the subject is present.
    #[serde(skip_serializing_if = "crate::output::is_false")]
    subject_absent: bool,
}

impl TagView {
    fn of(attributed: &AttributedTag,
          names: &BTreeMap<String, String>,
          office: &OfficeState,
          present: bool) -> TagView {
        let tagger = attributed.tagger.clone();

        TagView {
            name: attributed.tag.name.clone(),
            subject: attributed.tag.subject.clone(),
            message: attributed.tag.message.clone(),
            tagger_name: names.get(&tagger).cloned(),
            tagger_role: office.find_user(&tagger).map(|user| user.role.as_str().to_string()),
            tagger,
            signer: attributed.signer.clone(),
            tagged_at: render_timestamp(attributed.tag.tagged_at),
            subject_absent: !present,
        }
    }

    /// The tagger label: display name (or operator id), plus role when known. Sanitized: the
    /// display name is a remote directory lookup, so it can carry another operator's text.
    fn tagger_label(&self) -> String {
        let who = sanitize(self.tagger_name.as_deref().unwrap_or(&self.tagger));

        match &self.tagger_role {
            Some(role) => format!("{} ({})", who, role),
            None => who,
        }
    }
}

impl CommandOutput for TagView {
    fn render_human(&self) {
        println!("tag {}", self.name);
        println!("subject {}", self.subject);

        // Offers no command: no shipped verb fetches history for a pallet with no local ref.
        if self.subject_absent {
            println!(
                "warning: parcel {} is not in this store — never fetched here, or collected \
                after nothing else referenced it. Another copy of this warehouse may still \
                hold it; this store cannot restore it by itself.",
                self.subject
            );
        }

        println!("tagger  {}", self.tagger_label());
        println!("date    {}", render_display_date(&self.tagged_at));

        if !self.message.is_empty() {
            println!();

            for line in self.message.lines() {
                println!("    {}", sanitize(line));
            }
        }
    }
}

/// Render a Unix timestamp as RFC 3339 (UTC), the JSON form.
fn render_timestamp(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| seconds.to_string())
}

/// Render an RFC 3339 timestamp back to the human display format (`YYYY-MM-DD HH:MM:SS UTC`).
fn render_display_date(rfc3339: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(dt) => dt.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        Err(_) => rfc3339.to_string(),
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Created>(),
        crate::docgen::OutputSchema::of::<TagList>(),
        crate::docgen::OutputSchema::of::<TagView>(),
    ]
}
