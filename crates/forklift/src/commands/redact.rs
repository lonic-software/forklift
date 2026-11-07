use serde::Serialize;
use forklift_core::error::{sanitize, CoreError, RefusalCode};
use forklift_core::util::kv_utils::short;
use forklift_core::util::office_utils::Role;
use forklift_core::util::applied_redactions::{self, RedactionState, RedactionStatus};
use forklift_core::util::redaction_utils::{self, AttributedRedaction, HeadHolder, RedactedKind, Redaction};
use forklift_core::util::{config_utils, object_utils, pallet_utils, remote_utils};
use crate::cli::RedactAction;
use crate::commands::lower;
use crate::output::{self, CommandOutput};

/// Handle the redact command: permanent redactions of stored content. Without a subcommand, the
/// redactions are listed.
///
/// * `redact` / `redact list`             - List every redaction and where it stands.
/// * `redact record <target> -m <reason>` - Record a redaction (admin only), and apply it if it can.
/// * `redact apply`                       - Apply every redaction that can be now.
///
/// # Arguments
/// * `action` - The redact subcommand (`None` lists the redactions).
///
/// # Returns
/// * `Ok(())`      - If the command completed.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(action: Option<RedactAction>) -> Result<(), String> {
    match action {
        Some(RedactAction::Record { target, message }) => record(&target, &message),
        Some(RedactAction::Apply) => apply(),
        Some(RedactAction::List) | None => list(),
    }
}

/// Record a redaction of the object `target` names: a full object hash, or `<revision>:<path>`
/// for the content of one file at a revision. Under trust only an admin may record one, and the
/// object must be a file's content (a blob, or a chunked file's recipe). A hash some meta pallet's
/// history names is refused: that history is never rewritten, so the content could never go. So
/// is one a pallet head names, until the file is taken out of that head.
///
/// The admin rule and immutability are the pallet's own, checked on every write and by the
/// remote, and no head written after the redaction may name the content; the checks here only
/// refuse early, with a plainer message.
///
/// Once the cell commits, the redaction is applied if it can be (see [`applied_redactions`]). It
/// is recorded either way, so a failure to apply it does not fail the command: the redaction is
/// reported pending, with the reason, and `redact apply` tries again.
fn record(target: &str, reason: &str) -> Result<(), String> {
    let (hash, kind) = resolve_target(target)?;

    let operator = config_utils::get_operator()?;
    let writer = lower::meta_writer(&operator)?;

    if let Some(office) = writer.office() {
        let is_admin = office.find_user(&operator.identifier)
            .is_some_and(|user| matches!(user.role, Role::Admin));

        if !is_admin {
            return Err(format!(
                "Only an admin may redact content; \"{}\" is not an admin in this office.",
                operator.identifier
            ));
        }
    }

    let read = redaction_utils::read_redactions()?;

    if read.redactions.iter().any(|existing| existing.redaction.hash == hash)
        || read.unreadable.iter().any(|existing| existing.hash == hash) {
        return Err(format!("Object {} is already redacted; a redaction is permanent.", hash));
    }

    let holders = redaction_utils::meta_holders(&hash).map_err(nothing_recorded)?;

    if !holders.is_empty() {
        return Err(meta_held_refusal(&hash, &holders));
    }

    let chunks = match kind {
        RedactedKind::Blob => None,
        RedactedKind::Recipe => {
            let chunks = object_utils::recipe_chunk_hashes(&hash).map_err(|e| format!(
                "Object {} is a chunked file's recipe, and its chunk list cannot be read here \
                ({}); record the redaction in a warehouse that holds the recipe.",
                hash, e
            ))?;

            Some(redaction_utils::chunk_list_digest(&chunks))
        }
    };

    let redaction = Redaction {
        hash: hash.clone(),
        reason: reason.to_string(),
        redacted_at: chrono::Utc::now().timestamp(),
        chunks,
    };

    let holders = redaction_utils::head_holders(&redaction).map_err(nothing_recorded)?;

    if !holders.is_empty() {
        return Err(head_held_refusal(&hash, &holders));
    }

    let redactions_head = redaction_utils::record_redaction(&redaction, &writer, &operator)?;

    let state = redaction_utils::read_redactions()
        .and_then(|read| applied_redactions::apply(&read, Some(&hash)))
        .map(|statuses| statuses.into_iter().next().map(|status| status.state))
        .unwrap_or_else(|why| Some(RedactionState::Undecided(why)))
        .unwrap_or_else(|| RedactionState::Undecided(format!(
            "The redaction of {} is not readable at the new @redactions head.", hash
        )));

    output::emit(&Recorded {
        hash, chunked: kind == RedactedKind::Recipe, redactions_head, status: StateView::from(&state),
    });

    Ok(())
}

/// Resolve what a redaction names: a full object hash held in the local store, or the content of
/// the file at `<revision>:<path>`, held or not.
fn resolve_target(target: &str) -> Result<(String, RedactedKind), String> {
    if object_utils::is_object_hash(target) {
        let kind = redaction_utils::held_kind(target)?.ok_or(format!(
            "Object {} is not in this store, so what it is cannot be told; name the file as \
            \"<revision>:<path>\" instead.",
            target
        ))?;

        return Ok((target.to_string(), kind));
    }

    let (revision, path) = target.split_once(':').ok_or_else(|| format!(
        "\"{}\" is neither a full object hash nor \"<revision>:<path>\".",
        target
    ))?;

    if revision.is_empty() || path.is_empty() {
        return Err(format!("\"{}\" needs both a revision and a path around the \":\".", target));
    }

    let parcel_hash = pallet_utils::resolve_revision(revision)?;
    let tree_hash = object_utils::load_parcel(&parcel_hash)?.tree_hash;

    let Some((hash, entry_type)) = object_utils::resolve_tree_file(&tree_hash, path)? else {
        return Err(format!(
            "\"{}\" was not found at revision \"{}\" ({}).",
            path, revision, parcel_hash
        ));
    };

    let kind = redaction_utils::entry_kind(entry_type).ok_or(format!(
        "\"{}\" is a directory at revision \"{}\"; name one file.",
        path, revision
    ))?;

    Ok((hash, kind))
}

/// A scan error from before the cell is written, with that said.
fn nothing_recorded(error: String) -> String {
    format!("{} Nothing was recorded.", error)
}

/// The [`RefusalCode::RedactionBlocked`] refusal for `hash`, which the history of every meta
/// pallet in `holders` names.
fn meta_held_refusal(hash: &str, holders: &[String]) -> String {
    let names: Vec<String> = holders.iter().map(|name| format!("@{}", name)).collect();

    CoreError::refusal_with_details(
        RefusalCode::RedactionBlocked,
        format!(
            "Object {} is named by the history of {}. A meta pallet's history is never rewritten, \
            so this content cannot be removed and nothing was recorded. Treat the content as \
            exposed.",
            hash,
            names.join(", ")
        ),
        "rotate or revoke the exposed secret",
        serde_json::json!({ "hash": hash, "meta_pallets": names }),
    ).into()
}

/// The [`RefusalCode::HeadNamesContent`] refusal for `hash`, which the head of every pallet in
/// `holders` names. A redaction removes content from history only, so the file comes out of each
/// head first. The next step takes it out of the pallet this bay stands on when that is a holder
/// (else the first), then records the redaction (see [`record_step`]).
fn head_held_refusal(hash: &str, holders: &[HeadHolder]) -> String {
    let position = pallet_utils::read_position().ok();
    let chosen = holders.iter()
        .find(|holder| position.as_ref().is_some_and(|position| position.pallet == holder.pallet))
        .unwrap_or(&holders[0]);
    let record = record_step(hash, chosen, holders);
    let then = match holders.len() {
        1 => record,
        _ => format!("take it out of the other heads in details.heads the same way, then {}", record),
    };
    let named: Vec<String> = holders.iter()
        .map(|holder| format!("pallet \"{}\" as \"{}\"", holder.pallet, holder.path))
        .collect();
    let heads: Vec<serde_json::Value> = holders.iter()
        .map(|holder| serde_json::json!({
            "pallet": holder.pallet, "head": holder.head, "path": holder.path,
        }))
        .collect();

    CoreError::refusal_with_details(
        RefusalCode::HeadNamesContent,
        format!(
            "Object {} is in the head of {}. A redaction removes content from history only, so no \
            pallet head may name it when the redaction is recorded, and nothing was recorded.",
            hash,
            named.join(", ")
        ),
        remote_utils::take_out_next_step(&chosen.pallet, &chosen.head, &chosen.path, &then),
        serde_json::json!({ "hash": hash, "heads": heads }),
    ).into()
}

/// The command that records the redaction of `hash` once the file is out of the heads: at a head
/// left behind, preferring `chosen`'s, where the path still names the file and resolves whether or
/// not the store holds it. A holder that matched by chunk list holds another recipe, which its
/// path would name instead, so with no holder of `hash` itself the hash is named bare.
fn record_step(hash: &str, chosen: &HeadHolder, holders: &[HeadHolder]) -> String {
    match std::iter::once(chosen).chain(holders).find(|holder| holder.hash == hash) {
        Some(exact) => redaction_utils::record_command(&exact.head, &exact.path),
        None => format!("forklift redact record {} -m <reason>", hash),
    }
}

/// List every redaction, attributed to its redactor, with where it stands, and every cell this
/// forklift cannot read. Applies nothing: a redaction that `redact apply` would apply lists as
/// ready.
fn list() -> Result<(), String> {
    let read = redaction_utils::read_redactions()?;
    let statuses = applied_redactions::statuses(&read)?;

    output::emit(&RedactionList::new(&read, &statuses));

    Ok(())
}

/// Apply every redaction that can be now, and list them all as [`list`] does.
fn apply() -> Result<(), String> {
    let read = redaction_utils::read_redactions()?;
    let statuses = applied_redactions::apply(&read, None)?;

    output::emit(&RedactionList::new(&read, &statuses));

    Ok(())
}

/// The result of recording a redaction.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Recorded {
    /// The redacted object.
    hash: String,

    /// Whether the object is a chunked file's recipe, whose chunk list is redacted with it.
    chunked: bool,

    /// The new head of the @redactions meta pallet.
    redactions_head: String,

    /// Where the redaction stands now.
    status: StateView,
}

impl CommandOutput for Recorded {
    fn render_human(&self) {
        let what = match self.chunked {
            true => "the chunked file",
            false => "object",
        };

        println!(
            "Recorded the redaction of {} {} (@redactions head {}).",
            what, self.hash, short(&self.redactions_head)
        );
        println!("It is {}.", self.status.describe());
    }
}

/// The list of redactions.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RedactionList {
    redactions: Vec<RedactionView>,

    /// Redactions this forklift cannot read — a newer schema, or a malformed cell. Listed, never
    /// dropped.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unreadable: Vec<UnreadableView>,
}

/// One redaction.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RedactionView {
    /// The redacted object.
    hash: String,

    /// Why it was redacted.
    reason: String,

    /// When it was recorded (RFC 3339, UTC).
    redacted_at: String,

    /// The redactor: an operator id, or a key id the office does not know.
    redactor: String,

    /// The key id the redaction is signed with; absent when it was written without a key.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,

    /// For a chunked file, the digest of its ordered chunk list.
    #[serde(skip_serializing_if = "Option::is_none")]
    chunks: Option<String>,

    /// Where the redaction stands.
    status: StateView,
}

impl RedactionList {
    /// The list of `read`'s redactions, each with its status from `statuses`.
    fn new(read: &redaction_utils::RedactionsRead, statuses: &[RedactionStatus]) -> RedactionList {
        let redactions = read.redactions.iter()
            .map(|attributed| {
                let state = statuses.iter()
                    .find(|status| status.hash == attributed.redaction.hash)
                    .map(|status| status.state.clone())
                    .unwrap_or_else(|| RedactionState::Undecided("It was not judged.".to_string()));

                RedactionView::new(attributed, &state)
            })
            .collect();
        let unreadable = read.unreadable.iter()
            .map(|cell| UnreadableView { hash: cell.hash.clone(), reason: cell.reason.clone() })
            .collect();

        RedactionList { redactions, unreadable }
    }
}

impl RedactionView {
    fn new(attributed: &AttributedRedaction, state: &RedactionState) -> Self {
        let redaction = &attributed.redaction;

        RedactionView {
            hash: redaction.hash.clone(),
            reason: redaction.reason.clone(),
            redacted_at: chrono::DateTime::from_timestamp(redaction.redacted_at, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| redaction.redacted_at.to_string()),
            redactor: attributed.redactor.clone(),
            signer: attributed.signer.clone(),
            chunks: redaction.chunks.clone(),
            status: StateView::from(state),
        }
    }
}

/// Where a redaction stands: applied, or pending, and why.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct StateView {
    /// `applied`; or pending as `held` (a pallet head or a meta pallet's history names it),
    /// `awaiting_remote` (nothing here names it, and the remote's report that it is applied there
    /// is waited for), `undecided` (whether anything names it could not be told) or `ready`
    /// (nothing here names it, and `redact apply` applies it; only `redact list` reports it).
    state: &'static str,

    /// For `held`, the pallet heads that name the content.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    heads: Vec<HeadView>,

    /// For `held`, the meta pallets whose history names the hash. That history is never
    /// rewritten, so the redaction stays pending.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_pallets: Vec<String>,

    /// For `undecided`, why.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// A pallet head that names a redaction's content.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HeadView {
    /// The pallet.
    pallet: String,

    /// Its head.
    head: String,

    /// Where the head's tree holds the content.
    path: String,
}

impl From<&RedactionState> for StateView {
    fn from(state: &RedactionState) -> Self {
        let view = |state| StateView { state, heads: Vec::new(), meta_pallets: Vec::new(), reason: None };

        match state {
            RedactionState::Applied => view("applied"),
            RedactionState::AwaitingRemote => view("awaiting_remote"),
            RedactionState::Ready => view("ready"),
            RedactionState::Undecided(reason) => StateView { reason: Some(reason.clone()), ..view("undecided") },
            RedactionState::Held { heads, meta_pallets } => StateView {
                heads: heads.iter()
                    .map(|holder| HeadView {
                        pallet: holder.pallet.clone(), head: holder.head.clone(), path: holder.path.clone(),
                    })
                    .collect(),
                meta_pallets: meta_pallets.iter().map(|name| format!("@{}", name)).collect(),
                ..view("held")
            },
        }
    }
}

impl StateView {
    /// The status as a phrase that follows "It is ". Pallet names and paths are another
    /// operator's text, so they are sanitized.
    fn describe(&self) -> String {
        match self.state {
            "applied" => "applied".to_string(),
            "awaiting_remote" => "pending until the remote reports it applied".to_string(),
            "ready" => "pending, and nothing names it any more: `forklift redact apply` applies it".to_string(),
            "held" => {
                let mut by: Vec<String> = self.heads.iter()
                    .map(|head| format!("pallet \"{}\" as \"{}\"", sanitize(&head.pallet), sanitize(&head.path)))
                    .collect();
                by.extend(self.meta_pallets.iter().map(|name| format!("the history of {}", sanitize(name))));

                format!("pending: named by {}", by.join(", "))
            }
            _ => format!("pending: {}", sanitize(self.reason.as_deref().unwrap_or("")).trim_end_matches('.')),
        }
    }
}

/// A redaction cell this forklift cannot read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct UnreadableView {
    /// The redacted object — the cell's path.
    hash: String,

    /// Why it cannot be read.
    reason: String,
}

impl CommandOutput for RedactionList {
    fn render_human(&self) {
        if self.redactions.is_empty() && self.unreadable.is_empty() {
            println!("No redactions.");
            return;
        }

        // The hash passed the content check, so it needs no sanitizing; the reason and redactor
        // are another operator's text and do.
        for redaction in &self.redactions {
            let signed = match redaction.signer {
                Some(_) => "",
                None => " (unsigned)",
            };

            println!(
                "{}  {}{}  {}",
                redaction.hash, sanitize(&redaction.redactor), signed, sanitize(&redaction.reason)
            );
            println!("    {}", redaction.status.describe());
        }

        for cell in &self.unreadable {
            println!("{}  (unreadable: {})", cell.hash, sanitize(&cell.reason));
        }
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Recorded>(),
        crate::docgen::OutputSchema::of::<RedactionList>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(pallet: &str, hash: &str) -> HeadHolder {
        HeadHolder {
            pallet: pallet.to_string(),
            head: format!("{}-head", pallet),
            path: format!("{}.bin", pallet),
            hash: hash.to_string(),
        }
    }

    #[test]
    fn the_record_step_names_a_head_only_where_it_holds_the_hash_itself() {
        let main = holder("main", "x");
        let side = holder("side", "y");

        assert_eq!(record_step("x", &main, &[main.clone(), side.clone()]),
                   "forklift redact record main-head:main.bin -m <reason>");
        assert_eq!(record_step("x", &side, &[main.clone(), side.clone()]),
                   "forklift redact record main-head:main.bin -m <reason>");
        assert_eq!(record_step("x", &side, std::slice::from_ref(&side)),
                   "forklift redact record x -m <reason>");

        let other = holder("other", "x");

        assert_eq!(record_step("x", &other, &[main.clone(), other.clone()]),
                   "forklift redact record other-head:other.bin -m <reason>");
    }
}
