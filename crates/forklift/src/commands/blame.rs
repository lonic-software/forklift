use std::collections::{BTreeMap, HashMap};
use serde::Serialize;
use forklift_core::model::parcel::Parcel;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::util::office_utils::OfficeState;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::{blame_utils, object_utils, office_utils, pallet_utils, remote_utils};
use crate::output::{self, CommandOutput};

/// Handle the blame command (git's "blame"/"annotate"): attribute every line of a file to
/// the parcel that introduced it, and — because authorship is signed and classed (§7.1) —
/// to the author's identity class and supervisor. That is the on-brand upgrade: a blame
/// that answers "was this line written by a human or an agent, under whose supervision",
/// offline and forge-proof.
///
/// The walk follows the first-parent chain from the revision (git's `blame --first-parent`);
/// a line a merge brought in from a side line is attributed to the merge parcel.
///
/// # Arguments
/// * `path`     - The warehouse path of the file to blame.
/// * `revision` - The revision to blame at (`None` blames the current pallet's head).
///
/// # Returns
/// * `Ok(())`      - If the blame was printed.
/// * `Err(String)` - If the revision cannot be resolved, the path is not a file there, or
///                   an object could not be read.
pub async fn handle_command(path: &str, revision: Option<String>) -> Result<(), String> {
    // An out-of-scope path is sealed by hash in a scoped bay; blaming it would hit an object
    // read the bay never materialized — refuse cleanly with a stable code instead (§7.6).
    crate::commands::scope::ensure_path_in_scope(WarehousePath::from_user_input(path)?.as_key())?;

    let head = match revision {
        Some(revision) => pallet_utils::resolve_revision(&revision)?,
        None => {
            let pallet = pallet_utils::get_current_pallet_name()?;

            pallet_utils::get_pallet_head(&pallet)?
                .ok_or(format!(
                    "Pallet \"{}\" has nothing stacked yet; there is nothing to blame.",
                    pallet
                ))?
        }
    };

    let file_blame = blame_utils::blame(&head, path)?;

    // Display names and identity classes are resolved exactly like `history`: the remote's
    // roster (best-effort, §8.12) supplies names, and the signed office (§7.1) supplies the
    // class and supervisor. Both are display sugar over forge-proof authorship.
    let names = remote_utils::resolve_office_display_names().await;
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    // Resolve each distinct blamed parcel once (a file has many lines, few parcels).
    let mut parcels: HashMap<String, BlamedParcel> = HashMap::new();

    for line in &file_blame.lines {
        if !parcels.contains_key(&line.parcel) {
            let resolved = BlamedParcel::resolve(&line.parcel, &names, &office)?;
            parcels.insert(line.parcel.clone(), resolved);
        }
    }

    let lines = file_blame.lines.iter().enumerate().map(|(index, line)| {
        BlameLine {
            number: index + 1,
            parcel: line.parcel.clone(),
            content: String::from_utf8_lossy(&line.content).trim_end_matches('\n').to_string(),
        }
    }).collect();

    output::emit("blame", &Blame {
        path: file_blame.path,
        revision: file_blame.revision,
        parcels,
        lines,
    });

    Ok(())
}

/// The blame of a file: its lines, and the parcels they are attributed to.
#[derive(Serialize)]
struct Blame {
    path: String,

    /// The revision the blame was taken at (the resolved head parcel hash).
    revision: String,

    /// The distinct blamed parcels, keyed by hash (so a line carries only the hash).
    parcels: HashMap<String, BlamedParcel>,

    lines: Vec<BlameLine>,
}

/// One line of the blamed file.
#[derive(Serialize)]
struct BlameLine {
    /// The 1-based line number.
    number: usize,

    /// The hash of the parcel that introduced the line (a key into `parcels`).
    parcel: String,

    /// The line content (without its trailing newline).
    content: String,
}

/// A parcel a line is attributed to, with its author resolved to signed identity metadata.
#[derive(Serialize)]
struct BlamedParcel {
    /// The primary author's pseudonymous operator id (the chain's record).
    operator: String,

    /// The resolved display name, when a resolution hook supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    /// The author's identity class (§7.1), when it is not a plain human — so agent, bot and
    /// service authorship is legible in the blame.
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    /// The supervising human of an automated author, when one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor: Option<String>,

    /// The author action's time as RFC 3339 (UTC).
    timestamp: String,
}

impl BlamedParcel {
    /// Resolve a parcel to its primary author and that author's signed identity metadata.
    fn resolve(hash: &str,
               names: &BTreeMap<String, String>,
               office: &OfficeState) -> Result<BlamedParcel, String> {
        let parcel = object_utils::load_parcel(hash)?;

        BlamedParcel::of(&parcel, names, office)
    }

    /// Build the resolved author from a parcel's actions: its primary (first) author, or —
    /// for a parcel with no author action — its stacker.
    fn of(parcel: &Parcel, names: &BTreeMap<String, String>, office: &OfficeState) -> Result<BlamedParcel, String> {
        let author = parcel.actions.iter()
            .find(|action| matches!(action.action, ParcelActionType::Author))
            .or_else(|| parcel.actions.first())
            .ok_or("A parcel has no actions; the warehouse may be corrupt.".to_string())?;

        let identifier = author.operator.identifier.clone();

        // A resolved name wins; the parser's identifier-as-name fallback is dropped.
        let name = names.get(&identifier).cloned().or_else(|| {
            (author.operator.name != identifier).then(|| author.operator.name.clone())
        });

        // The signed office record carries the class and supervisor (§7.1). A human (the
        // default) shows neither — only automated authorship is worth calling out.
        let user = office.find_user(&identifier);
        let class = user
            .map(|user| user.class)
            .filter(|class| class.is_automated())
            .map(|class| class.as_str().to_string());
        let supervisor = user.and_then(|user| user.supervisor.clone());

        Ok(BlamedParcel {
            operator: identifier,
            name,
            class,
            supervisor,
            timestamp: author.timestamp.to_rfc3339(),
        })
    }
}

impl CommandOutput for Blame {
    fn render_human(&self) {
        // Column widths: the author label and the line-number gutter, sized to the content.
        let author_width = self.lines.iter()
            .filter_map(|line| self.parcels.get(&line.parcel))
            .map(|parcel| parcel.author_label().len())
            .max()
            .unwrap_or(0);

        let number_width = self.lines.len().to_string().len();

        for line in &self.lines {
            let short = &line.parcel[..line.parcel.len().min(10)];

            let (author, date) = match self.parcels.get(&line.parcel) {
                Some(parcel) => (parcel.author_label(), parcel.short_date()),
                None => (String::from("?"), String::new()),
            };

            // git-blame layout: hash, author, date, line number, then the line.
            println!(
                "\x1b[33m{}\x1b[0m ({:<author_width$} {} {:>number_width$}) {}",
                short,
                author,
                date,
                line.number,
                line.content,
                author_width = author_width,
                number_width = number_width,
            );
        }
    }
}

impl BlamedParcel {
    /// The author label shown in the gutter: the display name (or operator id), plus a
    /// class marker for automated authors so a human/agent line is legible at a glance.
    fn author_label(&self) -> String {
        let who = self.name.clone().unwrap_or_else(|| self.operator.clone());

        match &self.class {
            Some(class) => {
                let supervisor = self.supervisor.as_ref()
                    .map(|s| format!(", supervised by {}", s))
                    .unwrap_or_default();

                format!("{} [{}{}]", who, class, supervisor)
            }
            None => who,
        }
    }

    /// The author date as `YYYY-MM-DD` for the gutter (the full time is in `--json`).
    fn short_date(&self) -> String {
        match chrono::DateTime::parse_from_rfc3339(&self.timestamp) {
            Ok(dt) => dt.with_timezone(&chrono::Utc).format("%Y-%m-%d").to_string(),
            Err(_) => self.timestamp.clone(),
        }
    }
}
