use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::util::office_utils::{OfficeState, Role};
use forklift_core::util::tag_utils::{AttributedTag, Tag};
use forklift_core::util::{config_utils, office_utils, pallet_utils, remote_utils, stack_utils, tag_utils};
use crate::cli::TagAction;
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
            create(&name, revision, message).await,
        Some(TagAction::Show { name }) => show(&name).await,
        Some(TagAction::List) | None => list().await,
    }
}

/// Create a signed tag pointing at a revision. A tag is a release marker, so — the on-brand
/// convention of §9.4d — it must be signed by an *admin* key: an authoritative act,
/// verifiable offline against the office chain. A tag name is immutable; a name already in
/// use is refused. Without a revision, the current pallet's head is tagged.
async fn create(name: &str, revision: Option<String>, message: Option<String>) -> Result<(), String> {
    tag_utils::validate_tag_name(name)?;

    let subject = match revision {
        Some(revision) => pallet_utils::resolve_revision(&revision)?,
        None => {
            let pallet = pallet_utils::get_current_pallet_name()?;

            pallet_utils::get_pallet_head(&pallet)?
                .ok_or(format!("Pallet \"{}\" has nothing stacked yet; there is nothing to tag.", pallet))?
        }
    };

    let operator = config_utils::get_operator()?;

    // A tag is signed post-metadata, so it needs an enrolled key — there is no unsigned tag.
    let signing_key_id = stack_utils::resolve_signing_key(&operator)?.ok_or(
        "A tag is signed and verifiable offline, so it needs an enrolled key. Establish \
        trust with \"office enroll\" first.".to_string()
    )?;

    // The release convention: only an admin may cut a tag (§9.4d).
    let office = office_utils::read_office_state()?;
    let is_admin = office.find_user(&operator.identifier)
        .map(|user| matches!(user.role, Role::Admin))
        .unwrap_or(false);

    if !is_admin {
        return Err(format!(
            "Only an admin may create a tag (a release is signed by an admin key, §9.4d); \
            \"{}\" is not an admin in this office.",
            operator.identifier
        ));
    }

    // A tag name is immutable: refuse one already reachable from the tags head.
    if let Some(existing) = tag_utils::find_tag(name)? {
        return Err(format!(
            "Tag \"{}\" already exists (points at {}); tags are immutable.",
            name, &existing.tag.subject[..existing.tag.subject.len().min(12)]
        ));
    }

    let tag = Tag {
        name: name.to_string(),
        subject: subject.clone(),
        message: message.unwrap_or_default(),
        tagged_at: chrono::Utc::now().timestamp(),
    };

    let parcel = tag_utils::record_tag(&tag, &operator, &signing_key_id)?;

    output::emit("tag", &Created {
        name: name.to_string(),
        subject,
        parcel,
    });

    Ok(())
}

/// List every tag, attributed to its (forge-proof) tagger.
async fn list() -> Result<(), String> {
    let tags = tag_utils::read_tags()?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let entries = tags.iter().map(|attributed| TagView::of(attributed, &names, &office)).collect();

    output::emit("tag", &TagList { tags: entries });

    Ok(())
}

/// Show one tag in full, verifying that it is signed against the office chain.
async fn show(name: &str) -> Result<(), String> {
    let attributed = tag_utils::find_tag(name)?
        .ok_or(format!("No tag named \"{}\" exists.", name))?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit("tag", &TagView::of(&attributed, &names, &office));

    Ok(())
}

/// The result of creating a tag.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Created {
    name: String,
    subject: String,

    /// The tag parcel on the @tags meta pallet.
    parcel: String,
}

impl CommandOutput for Created {
    fn render_human(&self) {
        println!(
            "Created tag \"{}\" -> {} (signed; @tags parcel {}).",
            self.name,
            &self.subject[..self.subject.len().min(12)],
            &self.parcel[..self.parcel.len().min(12)],
        );
    }
}

/// The list of tags.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct TagList {
    tags: Vec<TagView>,
}

impl CommandOutput for TagList {
    fn render_human(&self) {
        if self.tags.is_empty() {
            println!("No tags yet. Create one with \"forklift tag create <name> <revision>\".");
            return;
        }

        let width = self.tags.iter().map(|tag| tag.name.len()).max().unwrap_or(0);

        for tag in &self.tags {
            let message = tag.message.lines().next().filter(|line| !line.is_empty())
                .map(|line| format!("  {}", line))
                .unwrap_or_default();

            println!(
                "{:<width$}  {}  by {}{}",
                tag.name,
                &tag.subject[..tag.subject.len().min(12)],
                tag.tagger_label(),
                message,
                width = width,
            );
        }
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

    /// The tagger's pseudonymous operator id (the chain's record).
    tagger: String,

    /// The resolved display name, when a resolution hook supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    tagger_name: Option<String>,

    /// The tagger's role in the office, when known — so a reader can confirm the tag was
    /// cut by an admin (the release convention).
    #[serde(skip_serializing_if = "Option::is_none")]
    tagger_role: Option<String>,

    /// The tag creation time as RFC 3339 (UTC).
    tagged_at: String,

    /// The @tags parcel that introduced the tag.
    parcel: String,
}

impl TagView {
    fn of(attributed: &AttributedTag, names: &BTreeMap<String, String>, office: &OfficeState) -> TagView {
        let tagger = attributed.tagger.clone();
        let tagger_name = names.get(&tagger).cloned();
        let tagger_role = office.find_user(&tagger).map(|user| user.role.as_str().to_string());

        TagView {
            name: attributed.tag.name.clone(),
            subject: attributed.tag.subject.clone(),
            message: attributed.tag.message.clone(),
            tagger,
            tagger_name,
            tagger_role,
            tagged_at: render_timestamp(attributed.tag.tagged_at),
            parcel: attributed.parcel.clone(),
        }
    }

    /// The tagger label for the list: display name (or operator id), plus role when known.
    fn tagger_label(&self) -> String {
        let who = self.tagger_name.clone().unwrap_or_else(|| self.tagger.clone());

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
        println!("tagger  {}", self.tagger_label());
        println!("date    {}", render_display_date(&self.tagged_at));

        if !self.message.is_empty() {
            println!();

            for line in self.message.lines() {
                println!("    {}", line);
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
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Created", schemars::schema_for!(Created)),
        ("TagList", schemars::schema_for!(TagList)),
        ("TagView", schemars::schema_for!(TagView)),
    ]
}
