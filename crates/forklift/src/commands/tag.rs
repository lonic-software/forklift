use std::collections::BTreeMap;
use serde::Serialize;
use forklift_core::util::office_utils::{OfficeState, Role};
use forklift_core::util::tag_utils::{AttributedTag, Tag};
use forklift_core::util::{config_utils, file_utils, office_utils, pallet_utils, remote_utils, stack_utils, tag_utils};
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
///
/// A list is an enumeration, not a claim about one referent — a franchised or sparse store
/// legitimately holds `@tags` without every subject's pallet (see [`probe_subject_absent`]), so
/// a *definitively* absent subject degrades the row (`subject_absent`) rather than fails the
/// command; failing would brick listing on a store working exactly as designed. An
/// *indeterminate* probe is a different question and is propagated as an ordinary command
/// failure instead — see [`probe_subject_absent`] for why, and for the one case that is
/// actually reachable here.
async fn list() -> Result<(), String> {
    let tags = tag_utils::read_tags()?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let entries = tags.iter()
        .map(|attributed| {
            let subject_absent = probe_subject_absent(&attributed.tag.subject)?;

            Ok(TagView::of(attributed, &names, &office, subject_absent))
        })
        .collect::<Result<Vec<_>, String>>()?;

    output::emit("tag", &TagList { tags: entries });

    Ok(())
}

/// Show one tag in full, verifying that it is signed against the office chain.
///
/// Renders even when the subject is absent — it does not refuse. `franchise`'s
/// `adopt_meta_pallets` brings `@tags` over in full while `fetch_history_scoped` fetches only
/// the resolved pallet's history (and a sparse `--only` franchise deliberately skips the
/// whole-store bundle that would otherwise mask this), so a perfectly healthy `--only` clone
/// legitimately holds tags whose subject was cut on a pallet it never fetched — and every field
/// this renders (name, message, tagger, date) comes straight from the `@tags` record itself,
/// never from the subject object. A hash-shaped local state cannot distinguish "collected here
/// after nothing else referenced it" from "never fetched here" — gc keeps no tombstone and
/// there is no fetch ledger — so a refusal would have to fire on both, including the healthy
/// one; marking the row instead of refusing is what stays honest without over-firing on a store
/// working exactly as designed. See [`probe_subject_absent`] for the probe itself.
async fn show(name: &str) -> Result<(), String> {
    let attributed = tag_utils::find_tag(name)?
        .ok_or(format!("No tag named \"{}\" exists.", name))?;

    let subject_absent = probe_subject_absent(&attributed.tag.subject)?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit("tag", &TagView::of(&attributed, &names, &office, subject_absent));

    Ok(())
}

/// Whether a tag's subject parcel is absent from this store, probed directly
/// (`file_utils::does_object_exist`) rather than assumed from anything else. Shared by `list`
/// (degrades a row) and `show` (marks a full render) — see each for what it does with a
/// definite `true`.
///
/// An indeterminate probe — `does_object_exist` returned `Err`, so presence could not be
/// determined at all — is a different question from a definite absence and is propagated as an
/// ordinary command failure rather than reported as `true`; the reachable, tested case is a
/// plain I/O error reading the loose-object path (see
/// `tag_list_fails_loudly_rather_than_mislabeling_a_subject_it_could_not_probe`).
/// `does_object_exist` also consults a durability-taint gate, a second real source of the same
/// kind of failure *in that function*, but it is not reachable from here: the gate is
/// process-local, set only by a write *this same process* performed and failed to sync
/// (`taint_utils::gate_check`'s own doc comment) — and nothing in `tag`'s own commands writes
/// before this probe runs, so no such write is ever this process's to have failed. A
/// disk-recorded taint left standing by an earlier process is instead intercepted by
/// `main.rs`'s entry-heal chokepoint before any command's body — including this probe's — ever
/// runs.
fn probe_subject_absent(subject: &str) -> Result<bool, String> {
    Ok(!file_utils::does_object_exist(subject)?)
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

            let absent_marker =
                if tag.subject_absent == Some(true) { "  (subject not in this store)" } else { "" };

            println!(
                "{:<width$}  {}  by {}{}{}",
                tag.name,
                &tag.subject[..tag.subject.len().min(12)],
                tag.tagger_label(),
                message,
                absent_marker,
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

    /// Whether the subject parcel is not present in this store (never fetched here, or
    /// collected after nothing else referenced it — see [`probe_subject_absent`]). `Some(true)`
    /// when absent, `None` when present — never `Some(false)` — so the common row/render shape
    /// (subject present) is unchanged; matches the [`Option`] + `skip_serializing_if` pattern
    /// `tagger_role` above already uses, which a plain `bool` cannot: a `bool` field is always
    /// present in the schema's `required` list regardless of `skip_serializing_if`, so it would
    /// declare a field the common case never actually emits.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_absent: Option<bool>,
}

impl TagView {
    fn of(
        attributed: &AttributedTag,
        names: &BTreeMap<String, String>,
        office: &OfficeState,
        subject_absent: bool,
    ) -> TagView {
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
            subject_absent: subject_absent.then_some(true),
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

        // A dedicated line, not a losable suffix on the subject line — `show` is a full render,
        // not a one-line row, so it can afford (and owes the reader) the longer, honest form.
        // Offers no command: there is no shipped verb that fetches history for a pallet with no
        // local ref (`shift` to an unreffed pallet refuses, and `lower` takes no pallet
        // argument), so naming one would be unexecutable advice.
        if self.subject_absent == Some(true) {
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
