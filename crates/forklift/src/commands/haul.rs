use serde::Serialize;
use forklift_core::util::haul_utils::{self, Haul, HaulStatus, ReviewVerdict};
use forklift_core::util::office_utils::{self, OfficeState};
use forklift_core::util::{config_utils, merge_utils, object_utils, pallet_utils};
use crate::cli::HaulAction;
use crate::commands::consolidate::{self, MergeStatus};
use crate::commands::office;
use crate::output::{self, CommandOutput};

/// Handle the haul command: reviewable merge proposals (pull requests) on the `@haul`
/// meta pallet. Every mutating action stacks a signed event, so authorship — and in
/// particular *who approved* — is forge-proof and carries the operator's identity class.
///
/// * `haul open --target … [--source …] --title …` - open a proposal.
/// * `haul list [--state …]`                        - list proposals.
/// * `haul show <id>`                               - one proposal in full.
/// * `haul comment | review | merge | close | reopen` - act on a proposal.
///
/// # Returns
/// * `Ok(())`      - If the command was handled.
/// * `Err(String)` - If trust is not established, the haul/pallet is unknown, or an
///                   object could not be read or signed.
pub async fn handle_command(action: Option<HaulAction>) -> Result<(), String> {
    match action {
        Some(HaulAction::Open { target, source, title, message }) =>
            open(&target, source, &title, message.unwrap_or_default()),
        Some(HaulAction::List { state }) => list(state),
        Some(HaulAction::Show { id }) => show(&id),
        Some(HaulAction::Comment { id, message }) => comment(&id, &message),
        Some(HaulAction::Review { id, request_changes, comment, message }) =>
            review(&id, request_changes, comment, message.unwrap_or_default()),
        Some(HaulAction::Merge { id }) => merge(&id).await,
        Some(HaulAction::Close { id }) => set_status(&id, true),
        Some(HaulAction::Reopen { id }) => set_status(&id, false),
        None => list(None),
    }
}

/// Open a haul proposing `source` (default: the current pallet) be merged into `target`.
fn open(target: &str, source: Option<String>, title: &str, description: String) -> Result<(), String> {
    let source = match source {
        Some(name) => name,
        None => pallet_utils::get_current_pallet_name()?,
    };

    if source == target {
        return Err("A haul's source and target must be different pallets.".to_string());
    }

    let head = pallet_utils::get_pallet_head(&source)?
        .ok_or(format!("Pallet \"{}\" has nothing stacked; there is nothing to propose.", source))?;

    if pallet_utils::get_pallet_head(target)?.is_none() && !pallet_utils::does_pallet_exist(target) {
        return Err(format!("No pallet named \"{}\" to merge into.", target));
    }

    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    let id = haul_utils::open_haul(&source, target, &head, title, &description, &actor, &signing_key_id)?;

    output::emit("haul", &Opened { id, source, target: target.to_string(), title: title.to_string() });

    Ok(())
}

/// List hauls, optionally filtered by state.
fn list(state: Option<String>) -> Result<(), String> {
    let wanted = state.as_deref().unwrap_or("open");

    let hauls: Vec<HaulSummary> = haul_utils::read_hauls()?
        .into_iter()
        .filter(|haul| wanted == "all" || status_name(&haul.status) == wanted)
        .map(HaulSummary::of)
        .collect();

    output::emit("haul", &HaulList { state: wanted.to_string(), hauls });

    Ok(())
}

/// Show one haul in full.
fn show(id: &str) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;

    // Best-effort identity classes, so an agent's review is distinguishable from a human's.
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit("haul", &HaulDetail::of(&haul, &office));

    Ok(())
}

/// Record a comment.
fn comment(id: &str, message: &str) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;
    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    haul_utils::record_comment(&haul.id, message, &actor, &signing_key_id)?;

    output::emit("haul", &Acted { id: haul.id, action: "commented".to_string() });

    Ok(())
}

/// Record a review (approve by default; `--request-changes` or `--comment` otherwise).
fn review(id: &str, request_changes: bool, comment: bool, message: String) -> Result<(), String> {
    let verdict = if request_changes {
        ReviewVerdict::RequestChanges
    } else if comment {
        ReviewVerdict::Comment
    } else {
        ReviewVerdict::Approve
    };

    let haul = haul_utils::find_haul(id)?;
    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    haul_utils::record_review(&haul.id, verdict, &message, &actor, &signing_key_id)?;

    output::emit("haul", &Acted { id: haul.id, action: format!("reviewed ({})", verdict.as_str()) });

    Ok(())
}

/// Close or reopen a haul.
fn set_status(id: &str, closed: bool) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;

    if let HaulStatus::Merged(_) = haul.status {
        return Err("A merged haul cannot be closed or reopened.".to_string());
    }

    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    haul_utils::record_closed(&haul.id, closed, &actor, &signing_key_id)?;

    output::emit("haul", &Acted {
        id: haul.id,
        action: if closed { "closed".to_string() } else { "reopened".to_string() },
    });

    Ok(())
}

/// Merge a haul: consolidate its source head into the target, then record the merge.
async fn merge(id: &str) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;

    match &haul.status {
        HaulStatus::Merged(_) => return Err("This haul is already merged.".to_string()),
        HaulStatus::Closed => return Err("This haul is closed; reopen it before merging.".to_string()),
        HaulStatus::Open => {}
    }

    let actor = config_utils::get_operator()?;
    let (_state, signing_key_id, _actor_id) = office::require_signing_actor(&actor)?;

    // The merge lands on the target pallet, so it must be the current one.
    let current = pallet_utils::get_current_pallet_name()?;
    if current != haul.target {
        return Err(format!(
            "Merging lands on the target pallet — shift to \"{}\" first, then \"haul merge\".",
            haul.target
        ));
    }

    let target_head = pallet_utils::get_pallet_head(&current)?
        .ok_or(format!("Target pallet \"{}\" has nothing stacked.", current))?;

    // Already contained (e.g. a conflicted merge that was resolved and stacked by hand, or
    // the source was consolidated another way): just record the merge.
    if merge_utils::is_ancestor(&haul.head, &target_head)? {
        haul_utils::record_merged(&haul.id, &target_head, &actor, &signing_key_id)?;
        output::emit("haul", &Merged { id: haul.id, merge_parcel: target_head, already: true });
        return Ok(());
    }

    let target_tree = object_utils::load_parcel(&target_head)?.tree_hash;
    if !consolidate::is_warehouse_clean(&target_tree).await? {
        return Err("The working directory has uncommitted changes; commit or restore them before merging.".to_string());
    }

    // The "already merged" shortcut above only ever touches the `@haul` meta-pallet
    // (`haul_utils::record_merged`), never the working inventory, so it needs no exemption. Past
    // this point the incomplete-load guard lives inside `merge_head_into_current` itself.
    match consolidate::merge_head_into_current(&current, &target_head, &haul.head, &haul.source, true).await? {
        MergeStatus::Merged(parcel) => {
            haul_utils::record_merged(&haul.id, &parcel, &actor, &signing_key_id)?;
            output::emit("haul", &Merged { id: haul.id, merge_parcel: parcel, already: false });
        }
        MergeStatus::Conflicts(paths) => {
            // The consolidation is left in progress; the haul stays open. Resolving and
            // stacking creates the merge parcel — re-run `haul merge` to record it.
            output::emit("haul", &MergeConflicts { id: haul.id, conflicts: paths });
        }
    }

    Ok(())
}

/// The wire status name of a haul.
fn status_name(status: &HaulStatus) -> &'static str {
    match status {
        HaulStatus::Open => "open",
        HaulStatus::Merged(_) => "merged",
        HaulStatus::Closed => "closed",
    }
}

// ── output types ────────────────────────────────────────────────────────────

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Opened {
    id: String,
    source: String,
    target: String,
    title: String,
}

impl CommandOutput for Opened {
    fn render_human(&self) {
        println!("Opened haul {} — \"{}\".", short(&self.id), self.title);
        println!("  {} → {}", self.source, self.target);
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Acted {
    id: String,
    action: String,
}

impl CommandOutput for Acted {
    fn render_human(&self) {
        println!("{} haul {}.", capitalize(&self.action), short(&self.id));
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Merged {
    id: String,
    merge_parcel: String,
    already: bool,
}

impl CommandOutput for Merged {
    fn render_human(&self) {
        if self.already {
            println!("Haul {} was already merged into its target; recorded.", short(&self.id));
        } else {
            println!("Merged haul {} — merge parcel {}.", short(&self.id), short(&self.merge_parcel));
        }
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct MergeConflicts {
    id: String,
    conflicts: Vec<String>,
}

impl CommandOutput for MergeConflicts {
    fn render_human(&self) {
        println!("Haul {} does not merge cleanly — {} conflict(s):", short(&self.id), self.conflicts.len());
        for path in &self.conflicts {
            println!("  {}", path);
        }
        println!("Resolve them, \"stack\", then re-run \"haul merge\" to record the merge.");
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HaulSummary {
    id: String,
    title: String,
    source: String,
    target: String,
    status: String,
    approvals: usize,
}

impl HaulSummary {
    fn of(haul: Haul) -> HaulSummary {
        HaulSummary {
            approvals: haul.reviews.iter().filter(|r| r.verdict == ReviewVerdict::Approve).count(),
            id: haul.id,
            title: haul.title,
            source: haul.source,
            target: haul.target,
            status: status_name(&haul.status).to_string(),
        }
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HaulList {
    state: String,
    hauls: Vec<HaulSummary>,
}

impl CommandOutput for HaulList {
    fn render_human(&self) {
        if self.hauls.is_empty() {
            println!("No {} hauls.", self.state);
            return;
        }

        for haul in &self.hauls {
            let approvals = if haul.approvals > 0 { format!(" · {} approval(s)", haul.approvals) } else { String::new() };
            println!(
                "{}  [{}]  {}  ({} → {}{})",
                short(&haul.id), haul.status, haul.title, haul.source, haul.target, approvals
            );
        }
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ReviewLine {
    author: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    verdict: String,
    body: String,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ThreadLine {
    author: String,
    kind: String,
    body: String,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HaulDetail {
    id: String,
    title: String,
    source: String,
    target: String,
    status: String,
    head: String,
    opened_by: String,
    description: String,
    reviews: Vec<ReviewLine>,
    thread: Vec<ThreadLine>,
}

impl HaulDetail {
    fn of(haul: &Haul, office: &OfficeState) -> HaulDetail {
        let reviews = haul.reviews.iter().map(|review| ReviewLine {
            class: office.find_user(&review.author)
                .map(|user| user.class)
                .filter(|class| class.is_automated())
                .map(|class| class.as_str().to_string()),
            author: review.author.clone(),
            verdict: review.verdict.as_str().to_string(),
            body: review.body.clone(),
        }).collect();

        let thread = haul.thread.iter().map(|attributed| ThreadLine {
            author: attributed.author.clone(),
            kind: attributed.event.kind.as_str().to_string(),
            body: attributed.event.body.clone(),
        }).collect();

        HaulDetail {
            id: haul.id.clone(),
            title: haul.title.clone(),
            source: haul.source.clone(),
            target: haul.target.clone(),
            status: status_name(&haul.status).to_string(),
            head: haul.head.clone(),
            opened_by: haul.opened_by.clone(),
            description: haul.description.clone(),
            reviews,
            thread,
        }
    }
}

impl CommandOutput for HaulDetail {
    fn render_human(&self) {
        println!("\x1b[33mhaul {}\x1b[0m — {}", short(&self.id), self.title);
        println!("{} → {}   [{}]", self.source, self.target, self.status);
        println!("opened by {}", self.opened_by);
        println!("head {}", short(&self.head));

        if !self.description.is_empty() {
            println!();
            for line in self.description.lines() {
                println!("    {}", line);
            }
        }

        if !self.reviews.is_empty() {
            println!("\nReviews:");
            for review in &self.reviews {
                let class = review.class.as_ref().map(|c| format!(" [{}]", c)).unwrap_or_default();
                let body = first_line(&review.body);
                println!("  {}{}: {}{}", review.author, class, review.verdict, body);
            }
        }

        let comments: Vec<&ThreadLine> = self.thread.iter().filter(|t| t.kind == "comment").collect();
        if !comments.is_empty() {
            println!("\nComments:");
            for comment in comments {
                println!("  {}: {}", comment.author, comment.body);
            }
        }
    }
}

/// A short display form of a haul or parcel id.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// The first line of a body, prefixed with a space (empty when the body is empty).
fn first_line(body: &str) -> String {
    match body.lines().next() {
        Some(line) if !line.is_empty() => format!(" — {}", line),
        _ => String::new(),
    }
}

/// Capitalize the first letter of a word.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Opened", schemars::schema_for!(Opened)),
        ("HaulList", schemars::schema_for!(HaulList)),
        ("HaulDetail", schemars::schema_for!(HaulDetail)),
        ("Acted", schemars::schema_for!(Acted)),
        ("Merged", schemars::schema_for!(Merged)),
        ("MergeConflicts", schemars::schema_for!(MergeConflicts)),
    ]
}
