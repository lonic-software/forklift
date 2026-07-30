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

    // A tag name is immutable: refuse one already reachable from the tags head. `existing` came
    // out of `@tags` (synced in wholesale, unvalidated — see `render_safe`'s doc comment), so its
    // subject is rendered the same untrusted way every other tag-record string is.
    if let Some(existing) = tag_utils::find_tag(name)? {
        return Err(format!(
            "Tag \"{}\" already exists (points at {}); tags are immutable.",
            name, render_safe(&truncate_chars(&existing.tag.subject, 12))
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
        // `subject`/`parcel` are hashes this process resolved and stored itself (never a foreign
        // record — `create` never renders one), so char-safe truncation here is defense in depth
        // rather than a reachable panic; `name` already passed `validate_tag_name` above `create`.
        println!(
            "Created tag \"{}\" -> {} (signed; @tags parcel {}).",
            self.name,
            truncate_chars(&self.subject, 12),
            truncate_chars(&self.parcel, 12),
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

        // Sanitized once here (not stored on `TagView`, which `--json` also reads — see
        // `render_safe`'s doc comment) so the column width matches what actually prints.
        let names: Vec<String> = self.tags.iter().map(|tag| render_safe(&tag.name)).collect();
        let width = names.iter().map(|name| name.chars().count()).max().unwrap_or(0);

        for (tag, name) in self.tags.iter().zip(names) {
            let message = tag.message.lines().next().filter(|line| !line.is_empty())
                .map(|line| format!("  {}", render_safe(line)))
                .unwrap_or_default();

            // Placed ahead of `tagger`/`message` — both remote-authored text — rather than
            // trailing the line: a message ending in text that imitates this marker (or, worse,
            // an ANSI escape) cannot forge or blank it if it never gets to render after it.
            let absent_marker =
                if tag.subject_absent == Some(true) { "(subject not in this store)  " } else { "" };

            // `name` itself is remote-authored and precedes everything else in the row,
            // including the marker above — sanitizing its control characters (done above) stops
            // it from restyling or splitting the row, but a *printable* forgery (spaces and
            // parens are enough to spell the marker text or a fake " by " boundary) is a
            // different threat `validate_tag_name`'s own charset already rules out for any name
            // this codebase ever creates. A name that fails it is foreign; flagged with a prefix
            // literal — printed by code, before anything `name` supplies — so nothing inside
            // `name` can ever precede it, unlike a position defined only in terms of `name`'s own
            // content.
            let name_prefix = if tag.name_invalid == Some(true) { "[invalid name] " } else { "" };

            println!(
                "{}{:<width$}  {}  {}by {}{}",
                name_prefix,
                name,
                render_safe(&truncate_chars(&tag.subject, 12)),
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

    /// Present and `true` when `name` does not meet the naming rules `tag create` enforces
    /// (letters, digits, `.`, `_`, `-` only); omitted otherwise. `@tags` syncs in wholesale
    /// (`franchise`/`lower`), and nothing validates a record's `name` on the way in — only
    /// `tag create` does, for a name of its own choosing — so a foreign or older client's record
    /// can carry one this build would never have created itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    name_invalid: Option<bool>,

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

        let name_invalid = tag_utils::validate_tag_name(&attributed.tag.name).is_err();

        TagView {
            name: attributed.tag.name.clone(),
            name_invalid: name_invalid.then_some(true),
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
    /// Sanitized (see [`render_safe`]): `tagger_name` is a remote directory lookup and `tagger`
    /// falls back to a signature's raw `key_id` when the office does not recognize it, so either
    /// can carry attacker-chosen text; `tagger_role` cannot — it is one of a fixed local set of
    /// strings ([`office_utils::Role::as_str`]), never interpolated user content.
    fn tagger_label(&self) -> String {
        let who = self.tagger_name.clone().unwrap_or_else(|| self.tagger.clone());
        let who = render_safe(&who);

        match &self.tagger_role {
            Some(role) => format!("{} ({})", who, role),
            None => who,
        }
    }
}

impl CommandOutput for TagView {
    fn render_human(&self) {
        let name = render_safe(&self.name);
        let subject = render_safe(&self.subject);

        // "tag <name>" is the very first line — an ANSI/SGR sequence in `name` would otherwise
        // land ahead of (and could restyle or blank) every warning line below it, including the
        // subject one; sanitizing `name` closes that regardless of which warning follows.
        println!("tag {}", name);

        // Surfaced the way a malformed subject is (never refused): `name` reaching this render
        // means `find_tag` already matched it against a name argument, so it is never rejected
        // here, only flagged — see `TagView::name_invalid`'s doc comment for why one can exist
        // at all despite `tag create` validating its own.
        if self.name_invalid == Some(true) {
            println!(
                "warning: this tag's recorded name does not meet the naming rules \"tag create\" \
                enforces (letters, digits, \".\", \"_\", \"-\" only) — shown exactly as recorded."
            );
        }

        println!("subject {}", subject);

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
                subject
            );
        } else if self.subject_absent == Some(true) {
            println!(
                "warning: parcel {} is not in this store — never fetched here, or collected \
                after nothing else referenced it. Another copy of this warehouse may still \
                hold it; this store cannot restore it by itself.",
                subject
            );
        }

        println!("tagger  {}", self.tagger_label());
        println!("date    {}", render_display_date(&self.tagged_at));

        if !self.message.is_empty() {
            println!();

            for line in self.message.lines() {
                println!("    {}", render_safe(line));
            }
        }
    }
}

/// Truncate `s` to at most `chars` **Unicode scalar values**, never bytes — `&s[..n]` on a byte
/// index that lands mid-codepoint panics. A tag's `subject`/`parcel` are ordinarily pure ASCII
/// hex, but a subject is not guaranteed to be: [`SubjectProbe::Malformed`] exists precisely
/// because a `@tags` record's subject is an unvalidated free string a foreign client can fill
/// with anything, multi-byte UTF-8 included. Matches the `.chars().take(n).collect()` idiom
/// `manifest.rs`'s and `haul.rs`'s own `short` helpers already use for the same reason, over
/// those commands' own remote-authored hashes/ids.
///
/// # Arguments
/// * `s` - The text to truncate.
/// * `chars` - The maximum number of characters to keep.
///
/// # Returns
/// * `s`, truncated to at most `chars` characters (unchanged if already shorter).
fn truncate_chars(s: &str, chars: usize) -> String {
    s.chars().take(chars).collect()
}

/// Neutralize every control character in `s` — replaced with a space, never dropped, so a
/// length or column width computed before this call still roughly matches what renders. Applied
/// to every remote-authored string a `tag` command prints for a human: `@tags` syncs in wholesale
/// (`franchise`'s `adopt_meta_pallets`), `parse_tag` validates none of `Tag`'s fields, and
/// `tag create` is the *only* code path that ever validates one (its own `name`, at creation) —
/// so a foreign or older client's record can carry a `name`, `subject`, or `message` containing
/// anything, and an unrecognized signature falls back to its raw, equally unvalidated `key_id`
/// (see `tagger_label`). `--json` output is unaffected: `TagView`'s stored fields are never
/// mutated by this, only the copies `render_human` prints — a `--json` consumer needs the real
/// bytes, and escaping is a terminal-rendering concern, not a wire-format one.
///
/// The same technique `forklift_core::error::sanitize` already uses for `CoreError`'s human
/// message/next-step (`char::is_control`, replace with a space) — not reused directly, since
/// that helper is private and tied specifically to `CoreError`'s frame-safety contract, a
/// different reason to want the same effect than this module's terminal-rendering one; making it
/// a general-purpose cross-crate utility for an unrelated concern would conflate the two.
/// Replace-with-space, not drop, both to match that precedent and because a run of controls
/// collapsing to nothing could accidentally weld two words together into a third, unintended one.
///
/// This single pass is also why an ANSI/SGR escape sequence needs no separate handling: every
/// such sequence opens with the ESC control byte (`\x1b`), and with that byte gone the rest of
/// the sequence (`[31m`, …) is inert — just printable text a terminal never treats specially. The
/// same pass catches a bare `\r` (which can move a real terminal's cursor back to the start of
/// the line without a matching `\n`, letting later text overwrite earlier text on screen) and an
/// embedded `\n` (which would otherwise split a `tag list` row — and the marker position it
/// depends on — across more than the one line the format assumes).
///
/// # Arguments
/// * `s` - The text to neutralize.
///
/// # Returns
/// * `s`, with every [`char::is_control`] character replaced by a single space.
fn render_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
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

    // --- Round 3: every `@tags` string is remote-authored and arbitrary — a `TagView`'s stored
    // fields (which `--json` serializes verbatim) are never sanitized; only what `render_human`
    // prints to a terminal is. These tests exercise `render_safe`/`truncate_chars` directly, plus
    // the `TagView` fields (`name_invalid`) that surface an anomaly rather than hide or reject
    // it, matching `subject_malformed`'s own precedent.

    #[test]
    fn render_safe_neutralizes_every_control_character_and_nothing_else() {
        // An ANSI/SGR sequence needs no special-case handling: stripping the leading ESC control
        // byte alone is enough to make the rest of it ("[31m", "[0m") inert, ordinary text.
        let ansi = "\x1b[31mHIDDEN\x1b[0m";
        assert_eq!(render_safe(ansi), " [31mHIDDEN [0m");
        assert!(!render_safe(ansi).contains('\x1b'), "the raw ESC byte must never survive");

        // A bare `\r` (cursor-to-line-start on a real terminal, no matching `\n`) and an embedded
        // `\n` (which would otherwise split one row into more than one line) are both neutralized
        // by the same pass, not special-cased.
        assert_eq!(render_safe("a\rb"), "a b");
        assert_eq!(render_safe("a\nb"), "a b");

        // Ordinary Unicode — including non-ASCII printable text, which is not a control character
        // — passes through untouched; this is a control-character filter, not an ASCII filter.
        assert_eq!(render_safe("héllo wörld"), "héllo wörld");
        assert_eq!(render_safe(""), "");
        assert_eq!(render_safe("plain text, no controls"), "plain text, no controls");
    }

    #[test]
    fn truncate_chars_is_byte_safe_on_multi_byte_input_and_never_panics() {
        // "é" is 2 bytes (U+00E9); 12 of them is 24 bytes — a byte-range `&s[..12]` would land
        // exactly mid-character (byte 12 sits inside the 7th "é") and panic before this fix.
        let multi_byte = "é".repeat(12);
        assert_eq!(truncate_chars(&multi_byte, 12), multi_byte, "12 chars of 12 is everything");
        assert_eq!(truncate_chars(&multi_byte, 5).chars().count(), 5);
        assert_eq!(truncate_chars(&multi_byte, 5), "é".repeat(5));

        // Shorter than the requested length: returned whole, not padded or panicking.
        assert_eq!(truncate_chars("ab", 12), "ab");
        assert_eq!(truncate_chars("", 12), "");
    }

    /// THE PANIC FINDING, PINNED DIRECTLY: a subject containing a multi-byte UTF-8 character,
    /// rendered by both `list` and `show`, must produce a row/render rather than abort the
    /// process. Unreachable through the shipped CLI for the same reason the malformed-subject
    /// cases above are (see this module's own doc comment): `tag create`'s two subject sources
    /// both verify pure-ASCII hash shape before returning, and `probe_subject` itself never
    /// slices a subject at all (only `render_human`/`TagList::render_human`/`Created::
    /// render_human`/`create`'s duplicate-name refusal do) — so this builds the `TagView`
    /// directly, past `probe_subject` and `TagView::of` both, exactly the state a foreign
    /// record's malformed, multi-byte subject would put it in.
    #[test]
    fn a_multi_byte_subject_renders_without_panicking() {
        // 11 ASCII bytes, then "é" (2 bytes) straddling byte offset 12 — the exact shape that
        // panicked `&subject[..subject.len().min(12)]` before this fix.
        let subject = format!("{}{}{}", "a".repeat(11), "é", "b".repeat(20));
        assert!(
            !subject.is_char_boundary(12),
            "the fixture must actually straddle byte offset 12, or this test proves nothing"
        );

        let view = TagView {
            name: "v1.0".to_string(),
            name_invalid: None,
            subject: subject.clone(),
            message: "release".to_string(),
            tagger: "spike@forklift".to_string(),
            tagger_name: None,
            tagger_role: None,
            tagged_at: render_timestamp(1_700_000_000),
            parcel: "b".repeat(64),
            subject_absent: Some(true),
            subject_malformed: true,
        };

        // Must not panic — that is the entire point. `render_human` (`show`'s full render) slices
        // the subject nowhere post-fix (it prints the sanitized value whole), but exercising it
        // guards the class, not just today's specific slice sites.
        view.render_human();

        // `TagList::render_human` (`list`'s row) does still truncate the subject to 12 characters
        // for display — the site the finding named directly.
        let list = TagList { tags: vec![view] };
        list.render_human();
    }

    #[test]
    fn tag_view_of_sets_name_invalid_only_for_a_name_tag_create_would_never_produce() {
        let names = BTreeMap::new();
        let office = empty_office();

        // `validate_tag_name` rejects a space and parens outright — a foreign record's name
        // containing either could never have come from this codebase's own `tag create`.
        let mut forged = malformed_tag(&"a".repeat(64));
        forged.name = "v9 (subject not in this store)  by x".to_string();
        assert!(
            tag_utils::validate_tag_name(&forged.name).is_err(),
            "the fixture must actually be invalid, or this test proves nothing"
        );

        let view = TagView::of(&attributed(forged), &names, &office, SubjectProbe::Present);
        assert_eq!(view.name_invalid, Some(true), "a name outside tag create's own charset must be flagged");

        let ordinary = malformed_tag(&"a".repeat(64)); // name: "v1.0", from `malformed_tag`
        let view = TagView::of(&attributed(ordinary), &names, &office, SubjectProbe::Present);
        assert_eq!(view.name_invalid, None, "an ordinary name must never be flagged");
    }
}
