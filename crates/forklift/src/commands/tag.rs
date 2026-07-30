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
/// legitimately holds `@tags` without every subject's pallet (see [`probe_subject`]), so a
/// *definitively* absent subject (including a malformed one — see [`probe_subject`]) degrades
/// the row (`subject_absent`) rather than fails the command; failing would brick listing on a
/// store working exactly as designed. An *indeterminate* probe is a different question and is
/// propagated as an ordinary command failure instead — see [`probe_subject`] for why, and for
/// the one case that is actually reachable here.
async fn list() -> Result<(), String> {
    let tags = tag_utils::read_tags()?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let entries = tags.iter()
        .map(|attributed| {
            let probe = probe_subject(&attributed.tag)?;

            Ok(TagView::of(attributed, &names, &office, probe))
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
/// working exactly as designed. See [`probe_subject`] for the probe itself, including the third,
/// non-hash-shaped case.
async fn show(name: &str) -> Result<(), String> {
    let attributed = tag_utils::find_tag(name)?
        .ok_or(format!("No tag named \"{}\" exists.", name))?;

    let probe = probe_subject(&attributed.tag)?;

    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit("tag", &TagView::of(&attributed, &names, &office, probe));

    Ok(())
}

/// What probing a tag's subject found: present, definitely absent, or not even hash-shaped.
enum SubjectProbe {
    /// The subject parcel is in this store.
    Present,

    /// The subject parcel is not in this store, but the subject is at least a well-formed
    /// object hash — never fetched here, or collected after nothing else referenced it.
    Absent,

    /// The subject is not shaped like an object hash at all (`file_utils::is_valid_object_hash`
    /// says no) — a `@tags` record can carry one because [`Tag::subject`] is an unvalidated
    /// free string a foreign or older client could have authored, synced in wholesale by
    /// `adopt_meta_pallets`. No copy of any warehouse could ever hold an object under a name
    /// that is not a hash, so this is also a *definite* absence — just for a different reason,
    /// one the reader must describe accurately rather than fold into "never fetched, or
    /// collected" (both are false for this case).
    Malformed,
}

impl SubjectProbe {
    /// Whether the subject is absent for either reason — the single bit `TagView::subject_absent`
    /// carries over the wire; a consumer that only wants "is it here" never needs the reason.
    fn is_absent(&self) -> bool {
        !matches!(self, SubjectProbe::Present)
    }
}

/// Probe a tag's subject, classifying it three ways rather than two. A malformed subject is
/// handled first and entirely locally (a string shape check, never touching the object store):
/// detected explicitly via [`file_utils::is_valid_object_hash`], never inferred from
/// [`file_utils::does_object_exist`]'s `Err` text, which is brittle and not what that text is
/// for. Only a hash-shaped subject goes on to the real presence probe
/// (`file_utils::does_object_exist`).
///
/// An indeterminate probe — `does_object_exist` returned `Err`, so presence could not be
/// determined at all for an otherwise well-formed hash — is a different question from either
/// definite-absence case and is propagated as an ordinary command failure, naming the tag so a
/// user can find the offending record (never anonymous — this is what a caller's `?` gets: an
/// error message this function has already built, not `does_object_exist`'s raw one). The
/// reachable, tested case is a plain I/O error reading the loose-object path (see
/// `tag_list_fails_loudly_rather_than_mislabeling_a_subject_it_could_not_probe`).
/// `does_object_exist` also consults a durability-taint gate, a second real source of the same
/// kind of failure *in that function*, but it is not reachable from here: the gate is
/// process-local, set only by a write *this same process* performed and failed to sync
/// (`taint_utils::gate_check`'s own doc comment) — and nothing in `tag`'s own commands writes
/// before this probe runs, so no such write is ever this process's to have failed. A
/// disk-recorded taint left standing by an earlier process is instead intercepted by
/// `main.rs`'s entry-heal chokepoint before any command's body — including this probe's — ever
/// runs.
fn probe_subject(tag: &Tag) -> Result<SubjectProbe, String> {
    if !file_utils::is_valid_object_hash(&tag.subject) {
        return Ok(SubjectProbe::Malformed);
    }

    match file_utils::does_object_exist(&tag.subject) {
        Ok(true) => Ok(SubjectProbe::Present),
        Ok(false) => Ok(SubjectProbe::Absent),
        Err(error) => Err(format!(
            "Could not determine whether tag \"{}\"'s subject ({}) is present in this store: {}",
            tag.name, tag.subject, error
        )),
    }
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

            // Placed ahead of `tagger`/`message` — both remote-authored text — rather than
            // trailing the line: a message ending in text that imitates this marker (or, worse,
            // an ANSI escape) cannot forge or blank it if it never gets to render after it.
            let absent_marker =
                if tag.subject_absent == Some(true) { "(subject not in this store)  " } else { "" };

            println!(
                "{:<width$}  {}  {}by {}{}",
                tag.name,
                &tag.subject[..tag.subject.len().min(12)],
                absent_marker,
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

    // `Option`, not a bare `bool`: `skip_serializing_if = "Option::is_none"` actually drops it
    // from the schema's `required` list, which a bare `bool` field cannot do regardless of
    // `skip_serializing_if` — the same pattern `tagger_role` above already uses. `Some(true)`
    // when absent, `None` when present — `Some(false)` is never constructed.
    /// Present and `true` when the subject parcel is not in this store (never fetched, collected,
    /// or recorded with an invalid hash); omitted when the subject is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_absent: Option<bool>,

    // Render-only: which wording `render_human`'s warning line uses when `subject_absent` is
    // `true`. Not part of the wire contract (`subject_absent` alone answers "is it here" for a
    // `--json` consumer) and never rendered by `TagList` — `#[serde(skip)]`, not
    // `skip_serializing_if`, so it never reaches the schema at all, unlike `subject_absent`
    // above before this fix.
    #[serde(skip)]
    subject_malformed: bool,
}

impl TagView {
    fn of(
        attributed: &AttributedTag,
        names: &BTreeMap<String, String>,
        office: &OfficeState,
        probe: SubjectProbe,
    ) -> TagView {
        let tagger = attributed.tagger.clone();
        let tagger_name = names.get(&tagger).cloned();
        let tagger_role = office.find_user(&tagger).map(|user| user.role.as_str().to_string());
        let subject_malformed = matches!(probe, SubjectProbe::Malformed);

        TagView {
            name: attributed.tag.name.clone(),
            subject: attributed.tag.subject.clone(),
            message: attributed.tag.message.clone(),
            tagger,
            tagger_name,
            tagger_role,
            tagged_at: render_timestamp(attributed.tag.tagged_at),
            parcel: attributed.parcel.clone(),
            subject_absent: probe.is_absent().then_some(true),
            subject_malformed,
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
        // argument), so naming one would be unexecutable advice. Two distinct wordings — never
        // fetched/collected is false for a malformed subject, so it gets its own honest line
        // rather than sharing the other's.
        if self.subject_malformed {
            println!(
                "warning: this tag's recorded subject, \"{}\", is not a valid parcel hash — no \
                copy of any warehouse could ever hold an object under that name.",
                self.subject
            );
        } else if self.subject_absent == Some(true) {
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

#[cfg(test)]
mod tests {
    //! A malformed (non-hash) subject cannot be reproduced end-to-end through the shipped CLI:
    //! `tag create`'s only two subject sources (`pallet_utils::resolve_revision`,
    //! `pallet_utils::get_pallet_head`) both already verify hash shape *and* object presence
    //! before returning, and no other production code path ever writes a `@tags` record — see
    //! `crates/forklift/tests/tag_subject_gc.rs`'s module doc comment for the fuller account of
    //! why (an on-disk fixture would mean reimplementing `LooseObject::store`'s compression by
    //! hand from the test, which is exactly the "reach into internals" this was told not to do).
    //! These unit tests instead pin [`probe_subject`]'s classification and [`TagView::of`]'s
    //! consequent field state directly — the type-level gap (`Tag.subject` is an unvalidated
    //! `String`) is real and worth covering even though only a foreign client, not this codebase,
    //! can currently produce the value.

    use super::*;

    fn malformed_tag(subject: &str) -> Tag {
        Tag {
            name: "v1.0".to_string(),
            subject: subject.to_string(),
            message: String::new(),
            tagged_at: 1_700_000_000,
        }
    }

    #[test]
    fn probe_subject_classifies_a_non_hash_subject_as_malformed_without_touching_the_store() {
        // No `StorageRootScope` entered at all: a malformed subject is caught by a pure string
        // check before `probe_subject` would ever need to consult the object store, so this
        // must not need one — if it did, this test would panic on a missing storage root instead
        // of asserting anything about classification. "abc" is deliberately NOT in this list: at
        // 3 hex characters it clears `is_valid_object_hash`'s permissive floor (it does not
        // enforce any particular digest's width, only "hex and long enough to fan out on") — a
        // short-but-hex-shaped subject is an ordinary Absent question, not a malformed one; see
        // the next test.
        for subject in ["not-a-valid-hash", "", "a", "ab", &"g".repeat(64)] {
            let probe = probe_subject(&malformed_tag(subject));

            assert!(
                matches!(probe, Ok(SubjectProbe::Malformed)),
                "\"{}\" is not hash-shaped and must classify as Malformed, not touch the store \
                or error; got {:?}",
                subject, probe.map(|_| "should be unreachable if Malformed").err()
            );
        }
    }

    /// A well-formed hex string that happens to name nothing is a different question — length +
    /// hex-ness alone is `probe_subject`'s definition of "malformed" (it does not check length
    /// against any particular digest's width), so a `Present`/`Absent` question for a
    /// hash-shaped-but-nonexistent subject must reach the real object-store probe, not the
    /// malformed short-circuit. This needs a real store, so it uses the same `tag_subject_gc.rs`
    /// integration fixture instead of a unit test here — see
    /// `a_signed_tag_subject_is_collected_after_undo_moves_the_head_back`.
    #[test]
    fn is_valid_object_hash_accepts_a_well_formed_hex_string_of_any_length_above_the_floor() {
        assert!(file_utils::is_valid_object_hash(&"a".repeat(64)));
        assert!(file_utils::is_valid_object_hash("abc123"));
        assert!(!file_utils::is_valid_object_hash("ab"));
        assert!(!file_utils::is_valid_object_hash(""));
        assert!(!file_utils::is_valid_object_hash("not-hex"));
    }

    fn attributed(tag: Tag) -> AttributedTag {
        AttributedTag { tag, tagger: "spike@forklift".to_string(), parcel: "b".repeat(64) }
    }

    fn empty_office() -> OfficeState {
        OfficeState { users: Vec::new(), keys: Vec::new() }
    }

    #[test]
    fn tag_view_of_a_malformed_subject_marks_absent_and_malformed_but_a_present_one_marks_neither() {
        let names = BTreeMap::new();
        let office = empty_office();

        let malformed = TagView::of(
            &attributed(malformed_tag("not-a-valid-hash")), &names, &office, SubjectProbe::Malformed,
        );
        assert_eq!(malformed.subject_absent, Some(true), "malformed must still mark subject_absent");
        assert!(malformed.subject_malformed, "malformed must set the render-only reason flag");

        let absent = TagView::of(
            &attributed(malformed_tag(&"a".repeat(64))), &names, &office, SubjectProbe::Absent,
        );
        assert_eq!(absent.subject_absent, Some(true), "a definite absence must mark subject_absent");
        assert!(!absent.subject_malformed, "an ordinary absence must not claim to be malformed");

        let present = TagView::of(
            &attributed(malformed_tag(&"a".repeat(64))), &names, &office, SubjectProbe::Present,
        );
        assert_eq!(present.subject_absent, None, "a present subject must omit subject_absent");
        assert!(!present.subject_malformed, "a present subject is never malformed");
    }

    /// Pins Finding 3's schema fix at the wire level, not just the Rust struct: the render-only
    /// `subject_malformed` field must never reach `--json` output at all (`#[serde(skip)]`, not
    /// `skip_serializing_if`) — a consumer only ever sees `subject_absent`.
    #[test]
    fn subject_malformed_never_appears_in_the_serialized_envelope() {
        let names = BTreeMap::new();
        let office = empty_office();
        let malformed = TagView::of(
            &attributed(malformed_tag("not-a-valid-hash")), &names, &office, SubjectProbe::Malformed,
        );

        let value = serde_json::to_value(&malformed).expect("TagView must serialize");
        let object = value.as_object().expect("TagView serializes as a JSON object");

        assert_eq!(object.get("subject_absent"), Some(&serde_json::Value::Bool(true)));
        assert!(
            !object.contains_key("subject_malformed"),
            "subject_malformed must never be serialized; keys were: {:?}",
            object.keys().collect::<Vec<_>>()
        );
    }
}
