use std::io::{Read, Write};
use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use forklift_core::model::parcel::Parcel;
use forklift_core::util::office_utils::{OfficeState, RevocationReason};
use forklift_core::util::query_utils::{
    self, MatchTrust, QueryMatch, QueryOutcome, QueryParams, TrustMode,
};
use forklift_core::util::{office_utils, pallet_utils, scope_utils};
use crate::output::{self, CommandOutput};

/// The query command's flag inputs, one struct so the CLI arm stays readable.
pub struct QueryArgs {
    pub revisions: Vec<String>,
    pub from: Option<String>,
    pub class: Option<String>,
    pub unsupervised: bool,
    pub supervisor: Option<String>,
    pub signer: Option<String>,
    pub author_after: Option<String>,
    pub author_before: Option<String>,
    pub merges: bool,
    pub no_merges: bool,
    pub grep: Option<String>,
    pub recorded: bool,
    pub model: Option<String>,
    pub tool: Option<String>,
    pub tag: Option<String>,
    pub touches: Option<String>,
    pub r#where: Option<String>,
    pub limit: Option<usize>,
    pub after: Option<String>,
    pub oneline: bool,
}

/// Handle the query command: filter parcel history on its signed dimensions (identity
/// class, supervisor, signing key) and parcel-local facts.
///
/// Identity answers are verified by default: the walk prunes only on non-identity
/// predicates, then resolves the *verified* signer (real signature check + key status) for
/// every survivor and filters on that — the parcel's own recorded operator never decides
/// what gets verified. `--recorded` opts into the cheap, self-declared reading; every
/// answer then says so.
pub async fn handle_command(args: QueryArgs) -> Result<(), String> {
    let predicate = build_predicate(&args)?;
    let trust = if args.recorded { TrustMode::Recorded } else { TrustMode::Verified };

    // Best-effort office, like history: a warehouse without trust shows no classes (and
    // verifies nothing — every parcel reads unsigned, honestly).
    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    // Seed the walk: a resumed cursor's frontier, the given revisions, or the current
    // pallet's head. An empty pallet yields the honest empty answer, not an error.
    let seeds: Vec<String> = match &args.after {
        Some(cursor) => {
            let hashes: Vec<&str> =
                cursor.split(',').map(str::trim).filter(|hash| !hash.is_empty()).collect();
            if hashes.is_empty() {
                return Err("The --after cursor is empty.".to_string());
            }
            hashes.iter().map(|hash| pallet_utils::resolve_revision(hash)).collect::<Result<_, _>>()?
        }
        None if !args.revisions.is_empty() => args
            .revisions
            .iter()
            .map(|revision| pallet_utils::resolve_revision(revision))
            .collect::<Result<_, _>>()?,
        None => {
            let pallet = pallet_utils::get_current_pallet_name()?;
            match pallet_utils::get_pallet_head(&pallet)? {
                Some(head) => vec![head],
                None => Vec::new(),
            }
        }
    };

    let from = args.from.as_deref().map(pallet_utils::resolve_revision).transpose()?;

    let params = QueryParams { seeds, from, predicate, trust, limit: args.limit };

    // The scope block rides every response so a consumer can never mistake a partial pass
    // for a complete one. `fetch_scope` appears only on a sparse warehouse.
    let fetch_scope = scope_utils::read_fetch_scope()
        .ok()
        .filter(|scope| !scope.is_full())
        .map(|scope| scope.prefixes().to_vec());

    if output::is_json() {
        let mut entries: Vec<QueryEntry> = Vec::new();
        let outcome = query_utils::run_query(&params, &office, |found| {
            entries.push(QueryEntry::of(&found, &office));
            true
        })?;

        let scope = scope_block(trust, &outcome, fetch_scope);
        output::emit("query", &QueryReport { matches: entries, next: outcome.next, scope });
        return Ok(());
    }

    // Human output streams match-by-match, so a quit pager or a closed `| head` stops the
    // walk and memory stays bounded. Buffered (256KiB) so the many small per-match writes
    // become a handful of `write` syscalls instead of one each.
    let mut out = std::io::BufWriter::with_capacity(256 * 1024, std::io::stdout().lock());
    let mut shown = 0usize;
    let mut revoked = 0usize;

    let outcome = query_utils::run_query(&params, &office, |found| {
        if found.identity.trust == MatchTrust::SignedRevoked {
            revoked += 1;
        }
        let rendered = if args.oneline {
            render_oneline(&mut out, &found)
        } else {
            render_match(&mut out, &found, &office, shown == 0)
        };
        shown += 1;
        rendered.is_ok()
    })?;

    // The trailing honesty note, printed when there is something to be honest about:
    // a recorded (unverified) pass, or matches signed by a revoked key.
    let mut notes: Vec<String> = Vec::new();
    if trust == TrustMode::Recorded {
        notes.push("identities are as recorded in the parcels, not verified".to_string());
    }
    if revoked > 0 {
        notes.push(format!("{} match(es) signed by a revoked key", revoked));
    }
    if !notes.is_empty() {
        let separator = if shown > 0 { "\n" } else { "" };
        let _ = writeln!(out, "{}note: {}.", separator, notes.join("; "));
    }

    let _ = outcome;

    // BufWriter's Drop flush swallows its error; flush explicitly and, matching the write
    // above, ignore a failure (the reader is gone, there's nothing left to do about it).
    let _ = out.flush();

    Ok(())
}

/// Desugar the CLI flags into the canonical JSON predicate tree and parse it through the
/// same validator `--where` uses, so flags and JSON can never drift in semantics.
fn build_predicate(args: &QueryArgs) -> Result<query_utils::Predicate, String> {
    let mut leaves: Vec<Value> = Vec::new();

    let leaf = |field: &str, op: &str, value: Value| json!({ "field": field, "op": op, "value": value });

    if let Some(class) = &args.class {
        leaves.push(leaf("author.class", "eq", json!(class)));
    }
    if args.unsupervised {
        leaves.push(leaf("author.class", "in", json!(["agent", "bot", "service"])));
        leaves.push(leaf("author.supervisor", "eq", Value::Null));
    }
    if let Some(supervisor) = &args.supervisor {
        leaves.push(leaf("author.supervisor", "eq", json!(supervisor)));
    }
    if let Some(signer) = &args.signer {
        leaves.push(leaf("signer.key", "eq", json!(signer)));
    }
    if let Some(after) = &args.author_after {
        leaves.push(leaf("author.date", "after", json!(after)));
    }
    if let Some(before) = &args.author_before {
        leaves.push(leaf("author.date", "before", json!(before)));
    }
    if args.merges {
        leaves.push(leaf("is_merge", "eq", json!(true)));
    }
    if args.no_merges {
        leaves.push(leaf("is_merge", "eq", json!(false)));
    }
    if let Some(grep) = &args.grep {
        leaves.push(leaf("description", "matches", json!(grep)));
    }
    if let Some(model) = &args.model {
        leaves.push(leaf("provenance.model", "matches", json!(model)));
    }
    if let Some(tool) = &args.tool {
        leaves.push(leaf("provenance.tool", "matches", json!(tool)));
    }
    if let Some(tag) = &args.tag {
        leaves.push(leaf("tag", "eq", json!(tag)));
    }
    if let Some(path) = &args.touches {
        leaves.push(leaf("path", "touches", json!(path)));
    }

    if let Some(payload) = &args.r#where {
        let payload = if payload == "-" {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|error| format!("Could not read the predicate from stdin: {}.", error))?;
            text
        } else {
            payload.clone()
        };

        // Parse the payload alone first (its own byte/shape bounds), then re-parse the
        // combined tree so the flag leaves count against the same structural bounds.
        query_utils::parse_where(&payload)?;
        let where_value: Value = serde_json::from_str(&payload)
            .expect("parse_where accepted this payload, so it is valid JSON");
        leaves.push(where_value);
    }

    query_utils::parse_value(&json!({ "all": leaves }))
}

fn scope_block(trust: TrustMode, outcome: &QueryOutcome, fetch_scope: Option<Vec<String>>) -> QueryScope {
    QueryScope {
        trust: match trust {
            TrustMode::Verified => "verified".to_string(),
            TrustMode::Recorded => "recorded".to_string(),
        },
        office_asof: "current".to_string(),
        walked: outcome.walked,
        matched: outcome.matched,
        out_of_scope: outcome.out_of_scope,
        provenance_source: if outcome.provenance_present { "present" } else { "meta_pallet_absent" }.to_string(),
        tags_source: if outcome.tags_present { "present" } else { "meta_pallet_absent" }.to_string(),
        fetch_scope,
    }
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

/// How many leading hash characters the terse form prints.
const ABBREV: usize = 12;

fn render_oneline(out: &mut impl Write, found: &QueryMatch) -> std::io::Result<()> {
    let subject = found
        .parcel
        .description
        .as_deref()
        .and_then(|description| description.lines().next())
        .unwrap_or("");
    let abbrev = &found.hash[..found.hash.len().min(ABBREV)];
    writeln!(out, "\x1b[33m{}\x1b[0m {}", abbrev, subject)
}

fn render_match(
    out: &mut impl Write,
    found: &QueryMatch,
    office: &OfficeState,
    is_first: bool,
) -> std::io::Result<()> {
    if !is_first {
        writeln!(out)?;
    }

    writeln!(out, "\x1b[33mparcel {}\x1b[0m", found.hash)?;

    // The identity line: who this parcel resolves to at the query's trust level.
    let identity = &found.identity;
    let operator = identity.operator.as_deref().unwrap_or("(unknown)");
    let mut qualifiers: Vec<String> = Vec::new();
    if let Some(class) = identity.class.filter(|class| class.is_automated()) {
        match &identity.supervisor {
            Some(supervisor) => {
                qualifiers.push(format!("{}, supervised by {}", class.as_str(), supervisor))
            }
            None => qualifiers.push(class.as_str().to_string()),
        }
    }
    let qualifiers = if qualifiers.is_empty() {
        String::new()
    } else {
        format!(" [{}]", qualifiers.join(", "))
    };
    let trust = match (identity.trust, identity.revocation_reason) {
        (MatchTrust::SignedRevoked, Some(reason)) => {
            format!("signed-revoked ({})", reason.as_str())
        }
        (trust, _) => trust.as_str().to_string(),
    };
    writeln!(out, "identity {}{} — {}", operator, qualifiers, trust)?;

    for action in &found.parcel.actions {
        let identifier = &action.operator.identifier;
        let user = office.find_user(identifier);
        let class = user
            .map(|user| user.class)
            .filter(|class| class.is_automated())
            .map(|class| {
                let supervisor = user
                    .and_then(|user| user.supervisor.as_deref())
                    .map(|supervisor| format!(", supervised by {}", supervisor))
                    .unwrap_or_default();
                format!(" [{}{}]", class.as_str(), supervisor)
            })
            .unwrap_or_default();

        writeln!(
            out,
            "{} {}{} at {}",
            action.action.get_name_for_peek(),
            identifier,
            class,
            action.timestamp.format("%Y-%m-%d %H:%M:%S UTC"),
        )?;
    }

    if let Some(description) = &found.parcel.description {
        writeln!(out)?;
        for line in description.lines() {
            writeln!(out, "    {}", line)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The --json report
// ---------------------------------------------------------------------------------------------

/// The query result: the matching parcels plus the honesty scope block.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryReport {
    matches: Vec<QueryEntry>,

    /// The cursor for the next `--json` page: pass it back as `--after` to resume. Absent
    /// once the history is exhausted. (Only meaningful with `-n`/`--limit`.)
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,

    /// What this pass covered and at what trust — always present, so a consumer can never
    /// mistake a partial or unverified pass for a complete, verified one.
    scope: QueryScope,
}

/// The honesty block: what the walk covered, at what trust, and what it could not see.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryScope {
    /// "verified" or "recorded" — the trust level identity answers were resolved at.
    trust: String,

    /// Office reads are a current snapshot: class/supervisor answers are "as recorded in
    /// the office today", not as of each parcel's authoring time. Always "current".
    office_asof: String,

    /// How many parcels the walk considered.
    walked: usize,

    /// How many parcels matched.
    matched: usize,

    /// Parcels a `touches` predicate could not confirm because the path was provably
    /// outside a sparse warehouse's fetch scope (degraded to `Unknown`, not an error). 0
    /// outside that case.
    out_of_scope: usize,

    /// Whether the `@manifest` meta pallet has a head at all: `"present"`, or
    /// `"meta_pallet_absent"` when it does not exist (or was never fetched) — every
    /// provenance leaf then reads `Unknown` for lack of a pallet to consult, not for lack
    /// of evidence on any one parcel.
    provenance_source: String,

    /// Whether the `@tags` meta pallet has a head at all: `"present"`, or
    /// `"meta_pallet_absent"` when it does not exist (or was never fetched) — mirrors
    /// `provenance_source`. A match's omitted `tags` only proves "genuinely untagged" when
    /// this reads `"present"`; when it reads `"meta_pallet_absent"` every `tags` omission is
    /// unknowable, not a negative result.
    tags_source: String,

    /// The warehouse's fetch-scope prefixes — present only on a sparse warehouse.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope: Option<Vec<String>>,
}

/// One matching parcel.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryEntry {
    parcel: String,

    /// The resolved author identity at the query's trust level. Under verified trust this
    /// is the verified signer (the only forge-proof attribution a parcel has); under
    /// recorded trust, the parcel's own claim.
    author: QueryIdentity,

    /// The verified signer, when the parcel carries a signature that verifies.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<QuerySigner>,

    is_merge: bool,

    /// The recorded per-action history (always the parcel's own claim, so each action is
    /// labeled trust "recorded" — the parcel-level `author` is where verification lives).
    actions: Vec<QueryAction>,

    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    /// This subject's newest machine-authorship provenance entry, if `@manifest` has any.
    /// Absent both when there is no entry for this parcel and when the whole pallet has no
    /// head — `scope.provenance_source` is what tells those two apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<QueryProvenance>,

    /// This subject's tag names (omitted, not empty-listed, when it carries none).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
}

/// The machine-authorship provenance a match carries, flattened for the report.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryProvenance {
    model: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

/// A resolved identity: operator, office class/supervisor, and the trust of the resolution.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryIdentity {
    /// The resolved operator id; absent when nothing resolves (unsigned / unknown key).
    #[serde(skip_serializing_if = "Option::is_none")]
    operator: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor: Option<String>,

    /// "verified" | "signed-revoked" | "unsigned" | "unknown-key" | "recorded".
    trust: String,
}

/// The verified signing key and its office binding.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QuerySigner {
    key: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    operator: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    /// Why the signing key was revoked — present exactly when the match is signed-revoked.
    #[serde(skip_serializing_if = "Option::is_none")]
    revocation_reason: Option<String>,
}

/// One recorded authorship/stack action (history-shaped, explicitly labeled recorded).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct QueryAction {
    action: String,

    /// The pseudonymous operator id the parcel records (self-declared).
    operator: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    class: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    supervisor: Option<String>,

    /// Always "recorded": a per-action identity is the parcel's own claim. The parcel-level
    /// `author` block carries the verified resolution.
    trust: String,

    #[serde(serialize_with = "serialize_rfc3339")]
    timestamp: DateTime<Utc>,
}

fn serialize_rfc3339<S: Serializer>(timestamp: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&timestamp.to_rfc3339())
}

impl QueryEntry {
    fn of(found: &QueryMatch, office: &OfficeState) -> QueryEntry {
        let identity = &found.identity;

        let signer = identity.signer_key.as_ref().map(|key| QuerySigner {
            key: key.clone(),
            operator: identity.operator.clone(),
            class: identity.class.map(|class| class.as_str().to_string()),
            revocation_reason: identity
                .revocation_reason
                .map(|reason: RevocationReason| reason.as_str().to_string()),
        });

        QueryEntry {
            parcel: found.hash.clone(),
            author: QueryIdentity {
                operator: identity.operator.clone(),
                class: identity.class.map(|class| class.as_str().to_string()),
                supervisor: identity.supervisor.clone(),
                trust: identity.trust.as_str().to_string(),
            },
            signer,
            is_merge: found.parcel.parents.len() > 1,
            actions: actions_of(&found.parcel, office),
            description: found.parcel.description.clone(),
            provenance: found.provenance.as_ref().map(|provenance| QueryProvenance {
                model: provenance.model.clone(),
                tool: provenance.tool.clone(),
                session: provenance.session.clone(),
            }),
            tags: found.tags.clone(),
        }
    }
}

fn actions_of(parcel: &Parcel, office: &OfficeState) -> Vec<QueryAction> {
    parcel
        .actions
        .iter()
        .map(|action| {
            let identifier = action.operator.identifier.clone();
            let user = office.find_user(&identifier);
            let class = user
                .map(|user| user.class)
                .filter(|class| class.is_automated())
                .map(|class| class.as_str().to_string());
            let supervisor = user.and_then(|user| user.supervisor.clone());

            QueryAction {
                action: action.action.get_name_for_peek().to_string(),
                operator: identifier,
                class,
                supervisor,
                trust: "recorded".to_string(),
                timestamp: action.timestamp,
            }
        })
        .collect()
}

impl CommandOutput for QueryReport {
    // Human mode streams directly (see `handle_command`); this buffered path serves any
    // caller that emits a fully-built report.
    fn render_human(&self) {
        println!("{} of {} walked parcels matched.", self.scope.matched, self.scope.walked);
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![("QueryReport", schemars::schema_for!(QueryReport))]
}
