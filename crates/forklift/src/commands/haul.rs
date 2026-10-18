use serde::Serialize;
use forklift_core::error::sanitize;
use forklift_core::util::haul_utils::{self, Haul, HaulStatus, ReviewVerdict, UnreadableCell};
use forklift_core::util::meta_tree::MetaSigner;
use forklift_core::util::office_utils::{self, OfficeState};
use forklift_core::util::{config_utils, merge_utils, object_utils, pallet_utils};
use crate::cli::HaulAction;
use crate::commands::consolidate::{self, MergeStatus};
use crate::commands::lower;
use crate::output::{self, CommandOutput};

/// Handle the haul command: reviewable merge proposals (pull requests) on the `@haul`
/// meta pallet. Every mutating action writes signed cells, so authorship — and in particular
/// *who approved* — is forge-proof and carries the operator's identity class.
///
/// * `haul open --target … [--source …] --title …` - open a proposal.
/// * `haul list [--state …]`                        - list proposals.
/// * `haul show <id>`                               - one proposal in full.
/// * `haul update <id>`                             - propose the source's current head.
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
        Some(HaulAction::Update { id }) => update(&id),
        Some(HaulAction::Comment { id, message }) => comment(&id, &message),
        Some(HaulAction::Review { id, request_changes, comment, message }) =>
            review(&id, request_changes, comment, message.unwrap_or_default()),
        Some(HaulAction::Merge { id }) => merge(&id).await,
        Some(HaulAction::Close { id }) => set_status(&id, HaulStatus::Closed),
        Some(HaulAction::Reopen { id }) => set_status(&id, HaulStatus::Open),
        None => list(None),
    }
}

/// The verified office and a signer for the configured operator.
fn signing() -> Result<(OfficeState, forklift_core::model::operator::Operator, MetaSigner<'static>), String> {
    let actor = config_utils::get_operator()?;
    let (office, signing_key_id) = lower::verified_signing_office(&actor)?;

    Ok((office, actor, MetaSigner::local(&signing_key_id)))
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

    let (office, actor, signer) = signing()?;
    let id = haul_utils::open_haul(&source, target, &head, title, &description, &office, &actor, &signer)?;

    output::emit(&Opened { id, source, target: target.to_string(), title: title.to_string() });

    Ok(())
}

/// List hauls, optionally filtered by state. A haul this build cannot read is listed beside
/// the rest, whatever the filter, since its state is unknown.
fn list(state: Option<String>) -> Result<(), String> {
    let wanted = state.as_deref().unwrap_or("open");
    let read = haul_utils::read_hauls()?;

    let hauls: Vec<HaulSummary> = read.hauls.iter()
        .filter(|haul| wanted == "all" || haul.status.as_str() == wanted)
        .map(HaulSummary::of)
        .collect();

    let unreadable = read.unreadable.into_iter()
        .map(|haul| UnreadableLine { path: haul.id, reason: haul.reason })
        .chain(read.stray.into_iter().map(UnreadableLine::of))
        .collect();

    output::emit(&HaulList { state: wanted.to_string(), hauls, unreadable });

    Ok(())
}

/// Show one haul in full.
fn show(id: &str) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;

    // Best-effort identity classes, so an agent's review is distinguishable from a human's.
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    output::emit(&HaulDetail::of(&haul, &office));

    Ok(())
}

/// A haul that is still open, for an action that only an open haul takes.
fn find_open(id: &str, action: &str) -> Result<Haul, String> {
    let haul = haul_utils::find_haul(id)?;

    match haul.status {
        HaulStatus::Open => Ok(haul),
        HaulStatus::Merged(_) => Err(format!("Haul {} is merged; a merged haul cannot be {}.", short(&haul.id), action)),
        HaulStatus::Closed => Err(format!("Haul {} is closed; reopen it before it is {}.", short(&haul.id), action)),
    }
}

/// Propose the source pallet's current head. Reviews of the earlier head stay recorded, but no
/// longer count for the haul.
fn update(id: &str) -> Result<(), String> {
    let haul = find_open(id, "updated")?;

    let head = pallet_utils::get_pallet_head(&haul.opened.source)?
        .ok_or(format!("Pallet \"{}\" has nothing stacked.", haul.opened.source))?;

    let (office, actor, signer) = signing()?;
    let changed = haul_utils::record_head(&haul.id, &head, &office, &actor, &signer)?.is_some();

    output::emit(&Updated { id: haul.id, head, changed });

    Ok(())
}

/// Record a comment.
fn comment(id: &str, message: &str) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;
    let (office, actor, signer) = signing()?;

    haul_utils::record_comment(&haul.id, message, &office, &actor, &signer)?;

    output::emit(&Acted { id: haul.id, action: "commented".to_string() });

    Ok(())
}

/// Record a review of the haul's current head (approve by default; `--request-changes` or
/// `--comment` otherwise).
fn review(id: &str, request_changes: bool, comment: bool, message: String) -> Result<(), String> {
    let verdict = if request_changes {
        ReviewVerdict::RequestChanges
    } else if comment {
        ReviewVerdict::Comment
    } else {
        ReviewVerdict::Approve
    };

    let haul = haul_utils::find_haul(id)?;
    let (office, actor, signer) = signing()?;

    haul_utils::record_review(&haul.id, verdict, &message, &haul.head, &office, &actor, &signer)?;

    output::emit(&Acted { id: haul.id, action: format!("reviewed ({})", verdict.as_str()) });

    Ok(())
}

/// Close or reopen a haul.
fn set_status(id: &str, status: HaulStatus) -> Result<(), String> {
    let haul = haul_utils::find_haul(id)?;

    match (&haul.status, &status) {
        (HaulStatus::Merged(_), _) => return Err("A merged haul cannot be closed or reopened.".to_string()),
        (HaulStatus::Closed, HaulStatus::Closed) => return Err(format!("Haul {} is already closed.", short(&haul.id))),
        (HaulStatus::Open, HaulStatus::Open) => return Err(format!("Haul {} is already open.", short(&haul.id))),
        _ => {}
    }

    let (office, actor, signer) = signing()?;
    haul_utils::record_status(&haul.id, &status, &office, &actor, &signer)?;

    output::emit(&Acted {
        id: haul.id,
        action: if status == HaulStatus::Closed { "closed".to_string() } else { "reopened".to_string() },
    });

    Ok(())
}

/// Merge a haul: consolidate its proposed head into the target, then record the merge.
async fn merge(id: &str) -> Result<(), String> {
    let haul = find_open(id, "merged")?;
    let (office, actor, signer) = signing()?;
    let record = |parcel: &str| haul_utils::record_status(&haul.id, &HaulStatus::Merged(parcel.to_string()), &office, &actor, &signer);

    // The merge lands on the target pallet, so it must be the current one.
    let current = pallet_utils::get_current_pallet_name()?;
    if current != haul.opened.target {
        return Err(format!(
            "Merging lands on the target pallet — shift to \"{}\" first, then \"haul merge\".",
            haul.opened.target
        ));
    }

    let target_head = pallet_utils::get_pallet_head(&current)?
        .ok_or(format!("Target pallet \"{}\" has nothing stacked.", current))?;

    // Already contained (e.g. a conflicted merge that was resolved and stacked by hand, or
    // the source was consolidated another way): just record the merge.
    if merge_utils::is_ancestor(&haul.head, &target_head)? {
        record(&target_head)?;
        output::emit(&Merged { id: haul.id.clone(), merge_parcel: target_head, already: true });
        return Ok(());
    }

    let target_tree = object_utils::load_parcel(&target_head)?.tree_hash;
    if !consolidate::is_warehouse_clean(&target_tree).await? {
        return Err("The working directory has uncommitted changes; commit or restore them before merging.".to_string());
    }

    // The "already merged" shortcut above only ever touches the `@haul` meta-pallet, never the
    // working inventory, so it needs no exemption. Past this point the incomplete-load guard
    // lives inside `merge_head_into_current` itself.
    match consolidate::merge_head_into_current(&current, &target_head, &haul.head, &haul.opened.source, true).await? {
        MergeStatus::Merged(parcel) => {
            record(&parcel)?;
            output::emit(&Merged { id: haul.id.clone(), merge_parcel: parcel, already: false });
        }
        MergeStatus::Conflicts(paths) => {
            // The consolidation is left in progress; the haul stays open. Resolving and
            // stacking creates the merge parcel — re-run `haul merge` to record it.
            output::emit(&MergeConflicts { id: haul.id.clone(), conflicts: paths });
        }
    }

    Ok(())
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
        println!("Opened haul {} — \"{}\".", short(&self.id), sanitize(&self.title));
        println!("  {} → {}", sanitize(&self.source), sanitize(&self.target));
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
pub(crate) struct Updated {
    id: String,
    head: String,

    /// False when the haul already proposed that head.
    changed: bool,
}

impl CommandOutput for Updated {
    fn render_human(&self) {
        if self.changed {
            println!("Haul {} now proposes {}.", short(&self.id), short(&self.head));
        } else {
            println!("Haul {} already proposes {}.", short(&self.id), short(&self.head));
        }
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

    /// Approvals of the haul's current head; an approval of an earlier head is not counted.
    approvals: usize,
}

impl HaulSummary {
    fn of(haul: &Haul) -> HaulSummary {
        HaulSummary {
            approvals: haul.current_reviews().filter(|r| r.verdict == ReviewVerdict::Approve).count(),
            id: haul.id.clone(),
            title: haul.opened.title.clone(),
            source: haul.opened.source.clone(),
            target: haul.opened.target.clone(),
            status: haul.status.as_str().to_string(),
        }
    }
}

/// A haul, or a cell of one, that this forklift cannot read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct UnreadableLine {
    /// The haul id, or the cell path within the pallet.
    path: String,
    reason: String,
}

impl UnreadableLine {
    fn of(cell: UnreadableCell) -> UnreadableLine {
        UnreadableLine { path: cell.path, reason: cell.reason }
    }
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HaulList {
    state: String,
    hauls: Vec<HaulSummary>,

    /// Hauls and cells this forklift cannot read, whatever the state filter.
    unreadable: Vec<UnreadableLine>,
}

impl CommandOutput for HaulList {
    fn render_human(&self) {
        if self.hauls.is_empty() {
            println!("No {} hauls.", self.state);
        }

        for haul in &self.hauls {
            let approvals = if haul.approvals > 0 { format!(" · {} approval(s)", haul.approvals) } else { String::new() };
            println!(
                "{}  [{}]  {}  ({} → {}{})",
                short(&haul.id), sanitize(&haul.status), sanitize(&haul.title), sanitize(&haul.source),
                sanitize(&haul.target), approvals
            );
        }

        render_unreadable(&self.unreadable);
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

    /// The head the review is of.
    head: String,

    /// Whether that is the haul's current head; a review of an earlier head no longer counts.
    current: bool,

    reviewed_at: i64,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct CommentLine {
    author: String,
    body: String,
    commented_at: i64,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HaulDetail {
    id: String,
    title: String,
    source: String,
    target: String,
    status: String,

    /// The merge parcel, when merged.
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_parcel: Option<String>,

    head: String,
    opener: String,
    opened_at: i64,
    description: String,
    reviews: Vec<ReviewLine>,

    /// Oldest first.
    comments: Vec<CommentLine>,

    /// The haul's review and comment cells this forklift cannot read.
    unreadable: Vec<UnreadableLine>,
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
            current: review.head == haul.head,
            head: review.head.clone(),
            reviewed_at: review.reviewed_at,
        }).collect();

        let comments = haul.comments.iter().map(|comment| CommentLine {
            author: comment.author.clone(),
            body: comment.body.clone(),
            commented_at: comment.commented_at,
        }).collect();

        HaulDetail {
            id: haul.id.clone(),
            title: haul.opened.title.clone(),
            source: haul.opened.source.clone(),
            target: haul.opened.target.clone(),
            status: haul.status.as_str().to_string(),
            merge_parcel: match &haul.status {
                HaulStatus::Merged(parcel) => Some(parcel.clone()),
                _ => None,
            },
            head: haul.head.clone(),
            opener: haul.opened.opener.clone(),
            opened_at: haul.opened.opened_at,
            description: haul.opened.description.clone(),
            reviews,
            comments,
            unreadable: haul.unreadable.iter().cloned().map(UnreadableLine::of).collect(),
        }
    }
}

impl CommandOutput for HaulDetail {
    fn render_human(&self) {
        // A haul syncs in from `@haul`, where nothing bounds its strings, so every one of them
        // is sanitized for the terminal (never for `--json`).
        println!("\x1b[33mhaul {}\x1b[0m — {}", short(&self.id), sanitize(&self.title));
        println!("{} → {}   [{}]", sanitize(&self.source), sanitize(&self.target), sanitize(&self.status));
        println!("opened by {}", sanitize(&self.opener));
        println!("head {}", short(&self.head));

        if let Some(parcel) = &self.merge_parcel {
            println!("merged as {}", short(parcel));
        }

        if !self.description.is_empty() {
            println!();
            for line in self.description.lines() {
                println!("    {}", sanitize(line));
            }
        }

        if !self.reviews.is_empty() {
            println!("\nReviews:");
            for review in &self.reviews {
                let class = review.class.as_ref().map(|c| format!(" [{}]", sanitize(c))).unwrap_or_default();
                let stale = if review.current { String::new() } else { format!(" (of {})", short(&review.head)) };
                let body = first_line(&review.body);
                println!("  {}{}: {}{}{}", sanitize(&review.author), class, sanitize(&review.verdict), stale, body);
            }
        }

        if !self.comments.is_empty() {
            println!("\nComments:");
            // A comment keeps its line structure: the first line beside the author, the rest
            // indented under it.
            for comment in &self.comments {
                let mut lines = comment.body.lines();
                println!("  {}: {}", sanitize(&comment.author), sanitize(lines.next().unwrap_or("")));
                for line in lines {
                    println!("      {}", sanitize(line));
                }
            }
        }

        render_unreadable(&self.unreadable);
    }
}

/// List what this forklift cannot read, after the rest.
fn render_unreadable(unreadable: &[UnreadableLine]) {
    if unreadable.is_empty() {
        return;
    }

    println!("\nCannot be read by this forklift (upgrade to read):");
    for line in unreadable {
        println!("  {}: {}", sanitize(&line.path), sanitize(&line.reason));
    }
}

/// A short, [sanitized](sanitize) display form of a haul or parcel id; both come out of a
/// synced record.
fn short(id: &str) -> String {
    sanitize(&id.chars().take(12).collect::<String>())
}

/// The first line of a body, [sanitized](sanitize) and prefixed with a dash (empty when the body
/// is empty).
fn first_line(body: &str) -> String {
    match body.lines().next() {
        Some(line) if !line.is_empty() => format!(" — {}", sanitize(line)),
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
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Opened>(),
        crate::docgen::OutputSchema::of::<HaulList>(),
        crate::docgen::OutputSchema::of::<HaulDetail>(),
        crate::docgen::OutputSchema::of::<Acted>(),
        crate::docgen::OutputSchema::of::<Updated>(),
        crate::docgen::OutputSchema::of::<Merged>(),
        crate::docgen::OutputSchema::of::<MergeConflicts>(),
    ]
}
