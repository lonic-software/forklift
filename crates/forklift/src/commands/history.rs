use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::io::Write;
use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use forklift_core::model::parcel::Parcel;
use forklift_core::util::office_utils::{IdentityClass, OfficeState};
use forklift_core::util::{object_utils, office_utils, pallet_utils, remote_utils};
use crate::output::{self, CommandOutput};

/// Handle the history command (git's "log"): walk the parcel graph from a revision and
/// print the parcels, newest first.
///
/// * `history`            - The history of the current pallet.
/// * `history <revision>` - The history from the given revision: a pallet name or a
///                          parcel hash (prefix).
///
/// Parcels are ordered by their latest action timestamp; the parents of a consolidation
/// are interleaved the same way (each parcel is printed once).
///
/// With `--class`, only parcels authored by an identity of that class are shown ("which
/// parcels did agents write", §7.1) — a display pass over the signed office records.
///
/// # Arguments
/// * `revision` - The revision to start from (`None` starts at the current pallet's head).
/// * `class`    - When set, keep only parcels an identity of this class authored.
/// * `limit`    - When set, stop after this many parcels are shown. Because the walk yields
///                parcels newest-first, a limit loads only the newest `limit` parcels and
///                their frontier — not the whole history (git log's default-recency behavior).
/// * `after`    - A pagination cursor (the `next` from a previous `--json` page): an opaque
///                comma-joined list of frontier parcels the walk resumes from. With `-n`,
///                this lets an agent read history in batches.
///
/// # Returns
/// * `Ok(())`      - If the history was printed.
/// * `Err(String)` - If the revision cannot be resolved or a parcel could not be read.
pub async fn handle_command(revision: Option<String>, class: Option<String>, limit: Option<usize>, after: Option<String>, oneline: bool) -> Result<(), String> {
    let class_filter = class.map(|value| IdentityClass::parse(&value)).transpose()?;

    // The terse human form (`--oneline`) prints only the abbreviated hash and subject, so it
    // needs neither display names nor — unless it is also filtering by class — the office. That
    // lets it skip the display-name resolution (a network round-trip) and the office read
    // entirely, which is much of what the verbose walk spends per parcel. It is a human render,
    // so `--json` (the machine format) ignores it.
    let terse = oneline && !output::is_json();

    // Display names, resolved through the configured remote (server-mediated per
    // §8.12): one batched call bounded by the office roster; no remote or any failure
    // falls back to the pseudonymous identifiers (display sugar, never verification).
    let names = if terse {
        BTreeMap::new()
    } else {
        remote_utils::resolve_office_display_names().await
    };

    // The office maps an operator to their signed identity class (§7.1); best-effort, so
    // a warehouse without trust simply shows no classes. Authorship stays forge-proof —
    // this only *reads* the class already recorded. Terse output shows no class, so it reads
    // the office only when a `--class` filter needs it.
    let office = if terse && class_filter.is_none() {
        OfficeState { users: Vec::new(), keys: Vec::new() }
    } else {
        office_utils::read_office_state()
            .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() })
    };

    // Seed the walk. `--after` resumes a previous --json page from its frontier (an opaque
    // comma-joined list of parcel hashes); otherwise start from the revision, or the head.
    let seeds: Vec<String> = match &after {
        Some(cursor) => {
            let hashes: Vec<&str> = cursor.split(',').map(str::trim).filter(|h| !h.is_empty()).collect();
            if hashes.is_empty() {
                return Err("The --after cursor is empty.".to_string());
            }
            hashes.iter().map(|h| pallet_utils::resolve_revision(h)).collect::<Result<_, _>>()?
        }
        None => vec![match revision {
            Some(revision) => pallet_utils::resolve_revision(&revision)?,
            None => {
                let pallet = pallet_utils::get_current_pallet_name()?;
                pallet_utils::get_pallet_head(&pallet)?.ok_or_else(|| output::empty_history(format!(
                    "Pallet \"{}\" has nothing stacked yet; there is no history.", pallet
                )))?
            }
        }],
    };

    // A max-heap on the latest action timestamp yields the parcels newest first, even
    // when a consolidation brings in a second line of history.
    let mut heap: BinaryHeap<(i64, String)> = BinaryHeap::new();
    let mut loaded: HashMap<String, Parcel> = HashMap::new();
    let mut enqueued: HashSet<String> = HashSet::new();

    for seed in seeds {
        if enqueued.insert(seed.clone()) {
            let parcel = object_utils::load_parcel(&seed)?;
            heap.push((latest_action_timestamp(&parcel), seed.clone()));
            loaded.insert(seed, parcel);
        }
    }

    if terse {
        return render_terse(heap, loaded, enqueued, class_filter, &office, limit);
    }

    // Human output streams entry-by-entry, so a quit pager or a closed `| head` stops the
    // walk and memory stays bounded; --json buffers one page and reports a `next` cursor.
    let streaming = !output::is_json();
    let mut sink = streaming.then(|| std::io::stdout().lock());

    let mut entries: Vec<HistoryEntry> = Vec::new();
    let mut shown: usize = 0;
    let mut reached_limit = false;

    while let Some((_, hash)) = heap.pop() {
        let parcel = loaded.remove(&hash).expect("every heap entry has its parcel loaded");
        let entry = HistoryEntry::of(&hash, &parcel, &names, &office);

        // The filter decides which parcels are shown, never which are walked.
        if class_filter.is_none_or(|class| entry.matches_class(class)) {
            match sink.as_mut() {
                // A write error means the reader (pager / pipe) went away: stop cleanly.
                Some(out) => if entry.render(out, shown == 0).is_err() { return Ok(()); },
                None => entries.push(entry),
            }
            shown += 1;
        }

        // Enqueue parents before the limit check so the frontier (the `next` cursor) is
        // complete when we stop.
        enqueue_parents(&parcel, &mut heap, &mut loaded, &mut enqueued)?;

        if limit.is_some_and(|n| shown >= n) {
            reached_limit = true;
            break;
        }
    }

    if streaming {
        return Ok(());
    }

    // The next page's cursor is the walk frontier — the parcels enqueued but not yet shown
    // (the heap's remaining hashes). Empty (or no limit hit) ⇒ the history is exhausted.
    let next = (reached_limit && !heap.is_empty()).then(|| {
        let mut frontier: Vec<String> = heap.into_iter().map(|(_, hash)| hash).collect();
        frontier.sort();
        frontier.join(",")
    });

    output::emit("history", &History { entries, next });

    Ok(())
}

/// The terse walk (`--oneline`): one line per parcel — the abbreviated hash and the
/// description's first line — streamed newest-first. It builds no `HistoryEntry` and touches the
/// office only when a `--class` filter needs it, so the per-parcel cost is a parcel read and a
/// short write, nothing more. Consumes the seeded walk state (heap/loaded/enqueued).
fn render_terse(
    mut heap: BinaryHeap<(i64, String)>,
    mut loaded: HashMap<String, Parcel>,
    mut enqueued: HashSet<String>,
    class_filter: Option<IdentityClass>,
    office: &OfficeState,
    limit: Option<usize>,
) -> Result<(), String> {
    /// How many leading hash characters the terse form prints (git abbreviates similarly).
    const ABBREV: usize = 12;

    let mut out = std::io::stdout().lock();
    let mut shown: usize = 0;

    while let Some((_, hash)) = heap.pop() {
        let parcel = loaded.remove(&hash).expect("every heap entry has its parcel loaded");

        // A class filter is rare with --oneline; only then is the (heavier) class lookup done.
        let show = class_filter.is_none_or(|class| {
            HistoryEntry::of(&hash, &parcel, &BTreeMap::new(), office).matches_class(class)
        });

        if show {
            let subject = parcel.description.as_deref()
                .and_then(|description| description.lines().next())
                .unwrap_or("");
            let abbrev = &hash[..hash.len().min(ABBREV)];
            // A write error means the reader (a pager, a `| head`) went away: stop cleanly.
            if writeln!(out, "\x1b[33m{}\x1b[0m {}", abbrev, subject).is_err() {
                return Ok(());
            }
            shown += 1;
        }

        enqueue_parents(&parcel, &mut heap, &mut loaded, &mut enqueued)?;

        if limit.is_some_and(|n| shown >= n) {
            break;
        }
    }

    Ok(())
}

/// Enqueue a parcel's parents into the walk (each parcel loaded once, shared history and
/// merges de-duplicated by `enqueued`).
fn enqueue_parents(
    parcel: &Parcel,
    heap: &mut BinaryHeap<(i64, String)>,
    loaded: &mut HashMap<String, Parcel>,
    enqueued: &mut HashSet<String>,
) -> Result<(), String> {
    for parent in &parcel.parents {
        if enqueued.insert(parent.clone()) {
            let parent_parcel = object_utils::load_parcel(parent)?;
            heap.push((latest_action_timestamp(&parent_parcel), parent.clone()));
            loaded.insert(parent.clone(), parent_parcel);
        }
    }
    Ok(())
}

/// The latest action timestamp of a parcel (in Unix seconds) — the moment it was
/// stacked, which is what the history is ordered by.
fn latest_action_timestamp(parcel: &Parcel) -> i64 {
    parcel.actions.iter()
        .map(|action| action.timestamp.timestamp())
        .max()
        .unwrap_or(0)
}

/// The parcel history: parcels newest first.
#[derive(Serialize)]
struct History {
    entries: Vec<HistoryEntry>,

    /// The cursor for the next `--json` page: pass it back as `--after` to resume. Absent
    /// once the history is exhausted. (Only meaningful with `-n`/`--limit`.)
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

/// One parcel in the history.
#[derive(Serialize)]
struct HistoryEntry {
    parcel: String,

    /// The parents a consolidation merges (present only for merge parcels).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    consolidates: Vec<String>,

    /// This parcel's parents, in their stored (canonical, base-first) order — always present,
    /// `[]` for a root parcel. Unlike `consolidates` (kept for compatibility, only non-empty on
    /// a merge), this is the graph edge a caller building a DAG needs regardless of parcel kind.
    parents: Vec<String>,

    actions: Vec<HistoryAction>,

    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// One authorship/stack action within a parcel.
#[derive(Serialize)]
struct HistoryAction {
    action: String,

    /// The pseudonymous operator id (always present — it is what the chain records).
    operator: String,

    /// The resolved display name, when a resolution hook supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,

    /// The operator's identity class (§7.1), when it is not a plain human — so agent,
    /// bot and service authorship is legible in the log.
    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    /// The supervising human of an automated identity, when one is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor: Option<String>,

    /// The action time. Serialized as RFC 3339 (UTC) for `--json`; formatted directly for
    /// the human log, so no timestamp is ever converted to a string and parsed back.
    #[serde(serialize_with = "serialize_rfc3339")]
    timestamp: DateTime<Utc>,
}

/// Serialize a timestamp as RFC 3339 (the machine-output format), without the round-trip
/// through a stored string that the human renderer used to parse back.
fn serialize_rfc3339<S: Serializer>(timestamp: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&timestamp.to_rfc3339())
}

impl HistoryEntry {
    fn of(hash: &str, parcel: &Parcel, names: &BTreeMap<String, String>, office: &OfficeState) -> HistoryEntry {
        let actions = parcel.actions.iter().map(|action| {
            let identifier = action.operator.identifier.clone();

            // A resolved name (the resolution hook) wins; the parser's
            // identifier-as-name fallback is not a real name and is dropped.
            let name = names.get(&identifier).cloned().or_else(|| {
                (action.operator.name != identifier).then(|| action.operator.name.clone())
            });

            // The signed office record carries the class and supervisor (§7.1). A human
            // (the default, and the fallback for an operator not in the office) shows
            // neither — only automated authorship is worth calling out.
            let user = office.find_user(&identifier);
            let class = user
                .map(|user| user.class)
                .filter(|class| class.is_automated())
                .map(|class| class.as_str().to_string());
            let supervisor = user.and_then(|user| user.supervisor.clone());

            HistoryAction {
                action: action.action.get_name_for_peek().to_string(),
                operator: identifier,
                name,
                class,
                supervisor,
                timestamp: action.timestamp,
            }
        }).collect();

        HistoryEntry {
            parcel: hash.to_string(),
            consolidates: if parcel.parents.len() > 1 { parcel.parents.clone() } else { Vec::new() },
            parents: parcel.parents.clone(),
            actions,
            description: parcel.description.clone(),
        }
    }

    /// Whether any action of this parcel was performed by an identity of the given class
    /// (an operator with no recorded class reads as a human).
    fn matches_class(&self, class: IdentityClass) -> bool {
        self.actions.iter()
            .any(|action| action.class.as_deref().unwrap_or("human") == class.as_str())
    }
}

impl HistoryEntry {
    /// Render one entry to `out`. A blank line separates entries, so `is_first` suppresses
    /// the leading one. Returns the write result so a streaming caller can stop cleanly when
    /// the reader (a pager, a `| head`) goes away.
    fn render(&self, out: &mut impl Write, is_first: bool) -> std::io::Result<()> {
        if !is_first {
            writeln!(out)?;
        }

        writeln!(out, "\x1b[33mparcel {}\x1b[0m", self.parcel)?;

        if !self.consolidates.is_empty() {
            writeln!(out, "consolidates {}", self.consolidates.join(" + "))?;
        }

        for action in &self.actions {
            let operator = match &action.name {
                Some(name) => format!("{} <{}>", name, action.operator),
                None => action.operator.clone(),
            };

            // Automated authorship is called out inline: "[agent, supervised by …]".
            let class = match &action.class {
                Some(class) => {
                    let supervisor = action.supervisor.as_ref()
                        .map(|s| format!(", supervised by {}", s))
                        .unwrap_or_default();
                    format!(" [{}{}]", class, supervisor)
                }
                None => String::new(),
            };

            writeln!(
                out,
                "{} {}{} at {}",
                action.action,
                operator,
                class,
                action.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
            )?;
        }

        if let Some(description) = &self.description {
            writeln!(out)?;

            for line in description.lines() {
                writeln!(out, "    {}", line)?;
            }
        }

        Ok(())
    }
}

impl CommandOutput for History {
    // The human walk streams entries directly (see `handle_command`); this buffered path
    // serves any caller that emits a fully-built `History`.
    fn render_human(&self) {
        let mut out = std::io::stdout().lock();

        for (index, entry) in self.entries.iter().enumerate() {
            if entry.render(&mut out, index == 0).is_err() {
                return;
            }
        }
    }
}

