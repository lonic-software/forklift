//! The parcel query engine (§9.4c): a walk-and-filter over parcel history whose predicates
//! reach the signed dimensions (identity class, supervisor, signer, provenance, tags, and
//! path history) that plain recorded metadata cannot prove.
//!
//! Provenance, tags and `touches` are phase-1 leaves, not identity leaves: a subject-hash
//! join against the separately-signed `@manifest`/`@tags` meta pallets (or a tree-diff
//! confirmed against the commit graph) is not spoofable the way a free-text action field
//! is, so there is no forgery-evasion reason to defer them past phase 1 the way `signer.*`
//! and `author.class` are. They still carry their own three-valued honesty rule, though:
//! provenance is *opt-in evidence* (absence is Unknown, never a negation — see
//! [`evaluate_leaf`]'s A5 note), while a tag is *membership* (absence is a plain False,
//! except when the whole `@tags` pallet is unreadable).
//!
//! The spine of the design is the trust guarantee, and it is an *execution order*, not just a
//! primitive choice: under verified trust (the default), identity predicates never prune the
//! candidate set on a recorded (attacker-writable) value. The walk runs in two phases per page:
//!
//! * **Phase 1** prunes on the non-identity predicates only (time, description, merge-ness,
//!   parcel hash, walk scope). An identity leaf evaluates to *unknown* here — three-valued
//!   logic — so a parcel is dropped only when the predicate is false no matter what the
//!   verified identity turns out to be.
//! * **Phase 2** resolves the verified signer identity ([`audit_utils::classify_parcel_trust`]
//!   — the real Ed25519 verify plus the key's active/revoked status) for **every** phase-1
//!   survivor, then applies the identity predicates against that resolution. The parcel body's
//!   self-declared operator never decides what gets verified.
//!
//! Under `--recorded` trust the caller opted into the weaker, cheaper guarantee: identity
//! predicates evaluate against the recorded operator in phase 1 (prune-first is sound there
//! because every result is labeled `recorded`), and no signature is read at all.
//!
//! Core never prints: the engine hands each match to a caller-supplied sink and returns typed
//! outcome data; the head renders.

use std::cell::{Cell, RefCell};
use std::collections::{BinaryHeap, HashMap, HashSet};
use chrono::{DateTime, Utc};
use serde_json::Value;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::error::{CoreError, RefusalCode};
use crate::model::parcel::Parcel;
use crate::util::audit_utils::{self, SignatureTrust};
use crate::util::office_utils::{IdentityClass, OfficeState, RevocationReason, Role};
use crate::util::path_utils::WarehousePath;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{
    fanout_utils, manifest_utils, merge_utils, object_utils, pallet_utils, tag_utils, tree_utils,
};

/// The stable code for a rejected predicate, re-exported for the head's error table.
pub const CODE_QUERY_PREDICATE_INVALID: &str = RefusalCode::QueryPredicateInvalid.as_str();

/// The maximum accepted `--where` payload, in bytes. A predicate tree has no legitimate
/// reason to be large; bounding the payload bounds the parse itself.
pub const MAX_WHERE_BYTES: usize = 64 * 1024;

/// The maximum predicate nesting depth (bounds the evaluator's recursion).
pub const MAX_PREDICATE_DEPTH: usize = 16;

/// The maximum total number of leaves (bounds per-parcel evaluation work).
pub const MAX_PREDICATE_LEAVES: usize = 128;

/// The maximum length of an `in` array (an unbounded set is an equivalent way to blow up
/// per-parcel comparison cost without tripping the leaf bound).
pub const MAX_IN_VALUES: usize = 256;

/// The maximum length of a `matches` glob, in characters (bounds matching cost; also a sane
/// ceiling for any real description/path/model string).
pub const MAX_GLOB_CHARS: usize = 256;

/// How trustworthy the identity answers are asked to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrustMode {
    /// The default: identity predicates and identity output resolve the *verified* signer
    /// (real signature check + key status) and never prune on a recorded value.
    Verified,

    /// The labeled opt-out: identity resolves the recorded (self-declared, forgeable)
    /// operator; cheap, prunes early, and every answer says `recorded`.
    Recorded,
}

/// The trust classification of one match's identity resolution — the vocabulary the output
/// carries. `Verified` and `SignedRevoked` are cryptographically bound; the rest are not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchTrust {
    /// A valid signature by a live office key.
    Verified,

    /// A valid signature by a revoked key — never flattened into `Verified`.
    SignedRevoked,

    /// The parcel carries no signature.
    Unsigned,

    /// Signed by a key the office does not track (the claimed key id is unverifiable).
    UnknownKey,

    /// The caller asked for recorded trust: the identity is the parcel's own claim.
    Recorded,
}

impl MatchTrust {
    /// The stable output value.
    pub fn as_str(self) -> &'static str {
        match self {
            MatchTrust::Verified => "verified",
            MatchTrust::SignedRevoked => "signed-revoked",
            MatchTrust::Unsigned => "unsigned",
            MatchTrust::UnknownKey => "unknown-key",
            MatchTrust::Recorded => "recorded",
        }
    }
}

/// Whether a signed-revoked match sits inside its revoking key's distrust boundary — present
/// exactly when [`IdentityResolution::trust`] is `SignedRevoked`. `Vouched` means the parcel
/// is part of the history the revoker vouched for at revocation time (the same reachability
/// test `audit` runs to accept a revoked-key signature at all — see
/// [`audit_utils::collect_reachable_present`]); `Suspect` means it sits outside that boundary
/// — a forged backdate, or the key's holder kept signing after the revocation. `audit` hard-
/// errors on a suspect parcel; a read-only query was only asked to read the history, so it
/// cannot refuse it — it labels the parcel loudly instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    Vouched,
    Suspect,
}

impl Boundary {
    /// The stable output value.
    pub fn as_str(self) -> &'static str {
        match self {
            Boundary::Vouched => "vouched",
            Boundary::Suspect => "suspect",
        }
    }
}

/// One match's resolved identity, at whatever trust the query ran with. Under verified trust
/// this is the signer's identity (the only forge-proof attribution a parcel has); under
/// recorded trust it is the first authoring action's self-declared operator.
pub struct IdentityResolution {
    pub trust: MatchTrust,

    /// The resolved operator id (the verified signer's, or the recorded author's). Absent
    /// when there is nothing to resolve (unsigned / unknown key).
    pub operator: Option<String>,

    /// The operator's identity class from the office, when the operator is enrolled.
    pub class: Option<IdentityClass>,

    /// The supervising operator, when the office records one.
    pub supervisor: Option<String>,

    /// The operator's role, when the operator is enrolled.
    pub role: Option<Role>,

    /// The signing key id, when the parcel is signed and the signature verifies.
    pub signer_key: Option<String>,

    /// Why the signing key was revoked — present exactly when `trust` is `SignedRevoked`.
    pub revocation_reason: Option<RevocationReason>,

    /// Whether a `SignedRevoked` match sits inside the revoking key's distrust boundary —
    /// present exactly when `trust` is `SignedRevoked`. Filled by [`QueryContext::fill_boundary`]
    /// on the main walk thread, after the resolution itself is built (`resolve_verified` never
    /// sets this — it has no `QueryContext` and runs inside `fanout_map`'s worker threads,
    /// where the boundary memo must not need a `Mutex`).
    pub boundary: Option<Boundary>,
}

/// One query match: the parcel and its identity resolution, ready to render.
pub struct QueryMatch {
    pub hash: String,
    pub parcel: Parcel,
    pub identity: IdentityResolution,

    /// This subject's newest provenance entry (by `recorded_at`), if `@manifest` has any —
    /// an entry-level fact for the report, independent of whether the predicate tested
    /// provenance at all (the index is always built; see [`QueryContext`]).
    pub provenance: Option<ProvenanceRecord>,

    /// This subject's tag names (empty when untagged, or when `@tags` has no head).
    pub tags: Vec<String>,
}

/// What a finished (or limit-stopped) query reports besides its matches.
pub struct QueryOutcome {
    /// The resume cursor (sorted, comma-joined frontier hashes); `None` when exhausted.
    pub next: Option<String>,

    /// How many parcels the walk considered.
    pub walked: usize,

    /// How many parcels matched (and were handed to the sink).
    pub matched: usize,

    /// How many `touches` confirmations degraded to `Unknown` because the path was provably
    /// outside the warehouse's fetch scope (§9.4c Finding 3) — always 0 outside a sparse
    /// warehouse running a `touches` predicate.
    pub out_of_scope: usize,

    /// Whether the `@manifest` meta pallet has a head at all (§9.4c A9). `false` means every
    /// provenance leaf read `Unknown` for lack of a pallet to consult, not for lack of
    /// evidence on any one parcel — the report must be able to tell the two apart.
    pub provenance_present: bool,

    /// Whether the `@tags` meta pallet has a head at all. `false` means every `tag` leaf
    /// read `Unknown` for lack of a pallet to consult — and, just as importantly, means a
    /// match's omitted `tags` is unknowable, not a proven "untagged": without this, a
    /// consumer reading an absent `tags` array cannot tell a genuinely untagged parcel from
    /// one queried before `@tags` ever existed.
    pub tags_present: bool,
}

/// One subject's flattened provenance evidence for the report: the newest entry by
/// `recorded_at` (a leaf itself matches on *any* of a subject's entries — see
/// [`evaluate_leaf`] — but the report shows one, the freshest attestation).
#[derive(Clone)]
pub struct ProvenanceRecord {
    pub model: String,
    pub tool: Option<String>,
    pub session: Option<String>,
    pub recorded_at: i64,
}

/// The ephemeral, per-query facts Stage 2's non-identity predicates need beyond the parcel
/// itself. Built once per query invocation (never persisted) because a warehouse's
/// provenance and tag records are tiny beside the history a query already walks — see
/// [`QueryContext::build`].
struct QueryContext {
    /// Subject hash → that subject's provenance entries. `None` when the `@manifest` meta
    /// pallet itself has no head — distinct from `Some(empty-for-this-subject)`, which is
    /// an ordinary parcel the pallet simply says nothing about.
    provenance: Option<HashMap<String, Vec<ProvenanceRecord>>>,

    /// Subject hash → tag names. `None` when the `@tags` meta pallet has no head.
    tags: Option<HashMap<String, Vec<String>>>,

    /// The warehouse's fetch scope, read once — `touches`'s confirming diff degrades to
    /// `Unknown` (rather than a hard error) only for a path this scope provably never
    /// fetched.
    fetch_scope: MaterializationScope,

    /// How many `touches` evaluations degraded per the above, across the whole walk.
    /// `Cell` because evaluation only ever borrows the context immutably.
    out_of_scope: Cell<usize>,

    /// Parcel hash → its `tree_hash`, memoized. `touches`'s confirming diff needs the first
    /// parent's tree hash, and the walk in `run_query` already loads (and decodes) every
    /// parent's full parcel body once to enqueue it — parcels deliberately bypass the shared
    /// read cache (see `object_utils::load_parcel`'s own doc), so without this memo a
    /// `touches` query would pay a second full uncached read + decode for nearly every
    /// walked parcel. `RefCell` because evaluation only ever borrows the context immutably,
    /// and this stays single-threaded (evaluation runs on the walk thread only, never inside
    /// the phase-2 fanout, which resolves identities, not tree hashes).
    tree_hashes: RefCell<HashMap<String, String>>,

    /// Revoked key id → that revocation's vouched parcel set (the same reachability
    /// primitive `audit` uses, [`audit_utils::collect_reachable_present`], over the key's
    /// `distrust_boundary`), memoized so a walk with many parcels signed by the same
    /// revoked key pays for one boundary walk, not one per parcel. Filled by
    /// [`Self::fill_boundary`] on the main walk thread only — never inside `fanout_map`'s
    /// worker threads — so this memo needs no `Mutex`.
    boundaries: RefCell<HashMap<String, HashSet<String>>>,
}

impl QueryContext {
    /// Build the ephemeral indexes: read `@manifest` and `@tags` once each (empty-but-present
    /// vs. absent-pallet is preserved — see the field docs), and the warehouse's fetch scope.
    fn build() -> Result<QueryContext, String> {
        let provenance = pallet_utils::get_meta_pallet_head(manifest_utils::MANIFEST_PALLET_NAME)?
            .is_some()
            .then(|| {
                let mut index: HashMap<String, Vec<ProvenanceRecord>> = HashMap::new();
                for attributed in manifest_utils::read_manifest()? {
                    if let Some(provenance) = attributed.entry.provenance {
                        index.entry(attributed.entry.subject).or_default().push(ProvenanceRecord {
                            model: provenance.model,
                            tool: provenance.tool,
                            session: provenance.session,
                            recorded_at: attributed.entry.recorded_at,
                        });
                    }
                }
                Ok::<_, String>(index)
            })
            .transpose()?;

        let tags = pallet_utils::get_meta_pallet_head(tag_utils::TAGS_PALLET_NAME)?
            .is_some()
            .then(|| {
                let mut index: HashMap<String, Vec<String>> = HashMap::new();
                for attributed in tag_utils::read_tags()? {
                    index.entry(attributed.tag.subject).or_default().push(attributed.tag.name);
                }
                Ok::<_, String>(index)
            })
            .transpose()?;

        Ok(QueryContext {
            provenance,
            tags,
            fetch_scope: scope_utils::read_fetch_scope()?,
            out_of_scope: Cell::new(0),
            tree_hashes: RefCell::new(HashMap::new()),
            boundaries: RefCell::new(HashMap::new()),
        })
    }

    /// The newest provenance record for a subject, if any (`None` distinguishes both "the
    /// pallet has no head" and "the pallet has one, but nothing about this subject" — a
    /// `Leaf::Provenance` test needs only the presence split; the report wants the record).
    fn newest_provenance(&self, hash: &str) -> Option<&ProvenanceRecord> {
        self.provenance.as_ref()?.get(hash)?.iter().max_by_key(|record| record.recorded_at)
    }

    fn tags_of(&self, hash: &str) -> &[String] {
        self.tags.as_ref().and_then(|index| index.get(hash)).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Record a parcel's tree hash into the memo — called wherever `run_query` already loads
    /// a parcel body (the seed loop, the parent-enqueue loop), so `touches` almost never has
    /// to load one itself.
    fn remember_tree_hash(&self, hash: &str, tree_hash: &str) {
        self.tree_hashes.borrow_mut().entry(hash.to_string()).or_insert_with(|| tree_hash.to_string());
    }

    /// The tree hash of a parcel, from the memo if present, else loaded (and memoized) on the
    /// spot — the fallback path for a parent already dropped from a previous page's frontier,
    /// or a seed the walk never separately enqueued.
    fn tree_hash_of(&self, hash: &str) -> Result<String, String> {
        if let Some(tree_hash) = self.tree_hashes.borrow().get(hash) {
            return Ok(tree_hash.clone());
        }
        let tree_hash = object_utils::load_parcel(hash)?.tree_hash;
        self.remember_tree_hash(hash, &tree_hash);
        Ok(tree_hash)
    }

    /// Fill a `SignedRevoked` resolution's `boundary`: is the parcel inside the revoking
    /// key's distrust boundary (`Vouched`) or outside it (`Suspect`)? Reuses `audit`'s own
    /// reachability primitive ([`audit_utils::collect_reachable_present`]) rather than
    /// reinventing the walk, memoized per distinct revoked key. A no-op for any other trust.
    ///
    /// Must run on the main walk thread, after a batch's identities are resolved and before
    /// `evaluate` reads them — never inside `fanout_utils::fanout_map`'s worker threads,
    /// which is exactly why this is a separate pass rather than something `resolve_verified`
    /// does inline.
    fn fill_boundary(&self, hash: &str, resolution: &mut IdentityResolution, office: &OfficeState) -> Result<(), String> {
        if resolution.trust != MatchTrust::SignedRevoked {
            return Ok(());
        }

        let key_id = resolution.signer_key.as_deref()
            .expect("a signed-revoked resolution always carries its signer key");
        let key = office.find_key(key_id)
            .expect("resolve_verified already found this key to classify the signature");

        if !self.boundaries.borrow().contains_key(key_id) {
            let vouched = audit_utils::collect_reachable_present(&key.distrust_boundary)?;
            self.boundaries.borrow_mut().insert(key_id.to_string(), vouched);
        }

        let vouched = self.boundaries.borrow().get(key_id).unwrap().contains(hash);
        resolution.boundary = Some(if vouched { Boundary::Vouched } else { Boundary::Suspect });
        Ok(())
    }
}

/// The query inputs. Seeds and `from` are already-resolved parcel hashes (revision
/// resolution is head-side, where the user's strings live).
pub struct QueryParams {
    /// The walk seeds (pallet heads, parcel hashes, or a resumed cursor's frontier).
    pub seeds: Vec<String>,

    /// Exclude this parcel and all its ancestors from the walk (the `A..B` scope shape).
    pub from: Option<String>,

    /// The predicate every reported parcel must satisfy.
    pub predicate: Predicate,

    pub trust: TrustMode,

    /// Stop after this many matches (the page size). Bounds output, never verification work
    /// over the walked scope.
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------------------------
// Predicate model and parsing
// ---------------------------------------------------------------------------------------------

/// A parsed, validated predicate tree.
pub enum Predicate {
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
    Leaf(Leaf),
}

/// Which actor an action-based leaf reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Author,
    Stacker,
}

/// A validated leaf test, pre-compiled so per-parcel evaluation never re-reads JSON.
pub enum Leaf {
    /// `author.operator` / `stacker.operator`: the recorded (self-declared) id — always
    /// phase 1, at every trust level; the field for "what does the parcel *claim*".
    RecordedOperator { kind: ActionKind, negate: bool, values: Vec<String> },

    /// `author.date` / `stacker.date`: action timestamps (Unix seconds), any-action match.
    Date { kind: ActionKind, after: Option<i64>, before: Option<i64> },

    /// `author.class` / `signer.class`: the identity class of the resolved identity.
    Class { negate: bool, values: Vec<IdentityClass> },

    /// `author.supervisor`: the resolved identity's supervisor; `None` in `values` tests
    /// "has no supervisor".
    Supervisor { negate: bool, values: Vec<Option<String>> },

    /// `author.role`: the resolved identity's office role.
    RoleIs { negate: bool, values: Vec<Role> },

    /// `signer.key`: the verified signing key, prefix-matched.
    SignerKey { prefixes: Vec<String> },

    /// `signer.operator`: the verified signing key's operator id.
    SignerOperator { negate: bool, values: Vec<String> },

    /// `signer.boundary`: for a `signed-revoked` match, whether the parcel sits inside the
    /// revoking key's distrust boundary (`vouched`) or outside it (`suspect`) — see
    /// [`Boundary`]. A live `verified` match reads `False` (definitively not revoked-key);
    /// anything without a forge-proof identity reads `Unknown`, same as every other signer
    /// leaf.
    SignerBoundary { values: Vec<Boundary> },

    /// `description`: glob/substring over the parcel description and action descriptions.
    Description { glob: String },

    /// `is_merge`: more than one parent.
    IsMerge { value: bool },

    /// `parents.count`: exact parent count.
    ParentsCount { value: usize },

    /// `parcel`: the parcel hash, prefix-matched.
    ParcelPrefix { prefixes: Vec<String> },

    /// `provenance.model` / `provenance.tool` / `provenance.session`: machine-authorship
    /// evidence from the `@manifest` meta pallet (§9.4c A5 — opt-in evidence, so absence is
    /// `Unknown`, never a negation match).
    Provenance { field: ProvenanceField, test: ProvenanceTest },

    /// `tag`: whether the parcel carries one of the named signed tags (from `@tags`) —
    /// membership, not evidence, so a parcel with no matching tag reads `False` (only the
    /// whole pallet being unreadable reads `Unknown`).
    Tag { values: Vec<String> },

    /// `path` / `touches`: whether the parcel's tree differs from its first parent's at a
    /// warehouse path (§9.4c Finding 3 — degrades to `Unknown` on a sparse warehouse whose
    /// fetch scope provably never covered the path).
    Touches { path: String },
}

/// Which field of a provenance entry a `provenance.*` leaf reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceField {
    Model,
    Tool,
    Session,
}

/// The comparison a `provenance.*` leaf runs — the same eq/ne/in-vs-glob split every other
/// string field in this engine offers, just named so `evaluate_leaf` can share one field
/// accessor between the two forms.
pub enum ProvenanceTest {
    Membership { negate: bool, values: Vec<String> },
    Matches { glob: String },
}

impl Leaf {
    /// Whether this leaf reads the resolved identity (phase 2 under verified trust) rather
    /// than parcel-local facts.
    fn is_identity(&self) -> bool {
        matches!(
            self,
            Leaf::Class { .. }
                | Leaf::Supervisor { .. }
                | Leaf::RoleIs { .. }
                | Leaf::SignerKey { .. }
                | Leaf::SignerOperator { .. }
                | Leaf::SignerBoundary { .. }
        )
    }

    /// Whether this leaf needs a signature to answer at all (signer facts have no recorded
    /// fallback: under recorded trust they are refused up front).
    fn is_signer(&self) -> bool {
        matches!(self, Leaf::SignerKey { .. } | Leaf::SignerOperator { .. } | Leaf::SignerBoundary { .. })
    }
}

impl Predicate {
    /// A predicate that matches everything (the no-filters query).
    pub fn everything() -> Predicate {
        Predicate::All(Vec::new())
    }

    fn any_leaf(&self, test: &impl Fn(&Leaf) -> bool) -> bool {
        match self {
            Predicate::All(children) | Predicate::Any(children) => {
                children.iter().any(|child| child.any_leaf(test))
            }
            Predicate::Not(child) => child.any_leaf(test),
            Predicate::Leaf(leaf) => test(leaf),
        }
    }

    /// Whether any leaf reads the resolved identity.
    pub fn has_identity_leaves(&self) -> bool {
        self.any_leaf(&Leaf::is_identity)
    }

    /// Whether any leaf reads signer facts (which have no recorded-trust fallback).
    pub fn has_signer_leaves(&self) -> bool {
        self.any_leaf(&Leaf::is_signer)
    }
}

/// Build the one refusal every rejected predicate maps to.
fn invalid(message: impl Into<String>) -> String {
    CoreError::refusal(
        RefusalCode::QueryPredicateInvalid,
        message,
        "Adjust the query predicate: combinators are \"all\", \"any\", \"not\"; a leaf is \
         {\"field\", \"op\", \"value\"}. See \"forklift help query\" for fields, operators \
         and bounds.",
    )
    .into()
}

/// Parse a `--where` payload (raw JSON text) into a validated predicate. Enforces the
/// payload byte bound before parsing and every structural bound during validation; all
/// failures — including `serde_json`'s own parse and recursion errors — are the one
/// predicate refusal, never a generic error.
pub fn parse_where(payload: &str) -> Result<Predicate, String> {
    if payload.len() > MAX_WHERE_BYTES {
        return Err(invalid(format!(
            "The predicate payload is {} bytes; the maximum is {} bytes.",
            payload.len(),
            MAX_WHERE_BYTES
        )));
    }

    let value: Value = serde_json::from_str(payload)
        .map_err(|error| invalid(format!("The predicate is not valid JSON: {}.", error)))?;

    parse_value(&value)
}

/// Parse an already-built JSON predicate tree (the head's flag desugaring goes through here
/// too, so flags and `--where` share one validator).
pub fn parse_value(value: &Value) -> Result<Predicate, String> {
    let mut leaves = 0usize;
    parse_node(value, 1, &mut leaves)
}

fn parse_node(value: &Value, depth: usize, leaves: &mut usize) -> Result<Predicate, String> {
    if depth > MAX_PREDICATE_DEPTH {
        return Err(invalid(format!(
            "The predicate nests deeper than the maximum depth of {}.",
            MAX_PREDICATE_DEPTH
        )));
    }

    let object = value.as_object().ok_or_else(|| {
        invalid("Every predicate node must be a JSON object.".to_string())
    })?;

    let combinators: Vec<&str> = ["all", "any", "not"]
        .into_iter()
        .filter(|key| object.contains_key(*key))
        .collect();

    match combinators.as_slice() {
        [] => {
            *leaves += 1;
            if *leaves > MAX_PREDICATE_LEAVES {
                return Err(invalid(format!(
                    "The predicate has more than the maximum of {} leaves.",
                    MAX_PREDICATE_LEAVES
                )));
            }
            parse_leaf(object).map(Predicate::Leaf)
        }

        [combinator] => {
            if object.len() != 1 {
                return Err(invalid(format!(
                    "A \"{}\" node must have no other keys.",
                    combinator
                )));
            }

            let inner = &object[*combinator];
            match *combinator {
                "not" => Ok(Predicate::Not(Box::new(parse_node(inner, depth + 1, leaves)?))),
                _ => {
                    let children = inner.as_array().ok_or_else(|| {
                        invalid(format!("\"{}\" takes an array of predicates.", combinator))
                    })?;
                    let parsed = children
                        .iter()
                        .map(|child| parse_node(child, depth + 1, leaves))
                        .collect::<Result<Vec<_>, _>>()?;
                    match *combinator {
                        "all" => Ok(Predicate::All(parsed)),
                        _ => Ok(Predicate::Any(parsed)),
                    }
                }
            }
        }

        _ => Err(invalid(
            "A predicate node combines with exactly one of \"all\", \"any\" or \"not\".".to_string(),
        )),
    }
}

/// The scalar values of an `eq`/`ne`/`in` leaf, unified: `eq` is a one-element set, `ne` a
/// negated one-element set, `in` a bounded set.
fn membership_values<'a>(
    object: &'a serde_json::Map<String, Value>,
    op: &str,
    field: &str,
) -> Result<(bool, Vec<&'a Value>), String> {
    let value = object
        .get("value")
        .ok_or_else(|| invalid(format!("The \"{}\" leaf is missing its \"value\".", field)))?;

    match op {
        "eq" => Ok((false, vec![value])),
        "ne" => Ok((true, vec![value])),
        "in" => {
            let items = value.as_array().ok_or_else(|| {
                invalid(format!("\"in\" on \"{}\" takes an array value.", field))
            })?;
            if items.len() > MAX_IN_VALUES {
                return Err(invalid(format!(
                    "The \"in\" array on \"{}\" has {} values; the maximum is {}.",
                    field,
                    items.len(),
                    MAX_IN_VALUES
                )));
            }
            Ok((false, items.iter().collect()))
        }
        other => Err(invalid(format!(
            "Operator \"{}\" does not apply to \"{}\" (expected \"eq\", \"ne\" or \"in\").",
            other, field
        ))),
    }
}

/// Every value must be a string; returns them owned.
fn string_values(values: Vec<&Value>, field: &str) -> Result<Vec<String>, String> {
    values
        .into_iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                invalid(format!("\"{}\" compares strings; got {}.", field, value))
            })
        })
        .collect()
}

/// Parse one RFC 3339 timestamp value.
fn timestamp(value: &Value, field: &str) -> Result<i64, String> {
    let text = value.as_str().ok_or_else(|| {
        invalid(format!("\"{}\" takes an RFC 3339 timestamp string.", field))
    })?;
    DateTime::parse_from_rfc3339(text)
        .map(|parsed| parsed.with_timezone(&Utc).timestamp())
        .map_err(|_| invalid(format!("\"{}\" is not an RFC 3339 timestamp on \"{}\".", text, field)))
}

fn parse_leaf(object: &serde_json::Map<String, Value>) -> Result<Leaf, String> {
    for key in object.keys() {
        if !matches!(key.as_str(), "field" | "op" | "value") {
            return Err(invalid(format!("Unknown key \"{}\" in a predicate leaf.", key)));
        }
    }

    let field = object
        .get("field")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("A predicate leaf needs a \"field\" string.".to_string()))?;
    let op = object
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("The \"{}\" leaf needs an \"op\" string.", field)))?;

    let date_leaf = |kind: ActionKind| -> Result<Leaf, String> {
        let value = object
            .get("value")
            .ok_or_else(|| invalid(format!("The \"{}\" leaf is missing its \"value\".", field)))?;
        match op {
            "before" => Ok(Leaf::Date { kind, after: None, before: Some(timestamp(value, field)?) }),
            "after" => Ok(Leaf::Date { kind, after: Some(timestamp(value, field)?), before: None }),
            "between" => {
                let bounds = value.as_array().filter(|bounds| bounds.len() == 2).ok_or_else(|| {
                    invalid(format!("\"between\" on \"{}\" takes a two-element array.", field))
                })?;
                // `between` is inclusive on both ends; before/after are exclusive, so widen
                // the bounds by one second each way.
                Ok(Leaf::Date {
                    kind,
                    after: Some(timestamp(&bounds[0], field)? - 1),
                    before: Some(timestamp(&bounds[1], field)? + 1),
                })
            }
            other => Err(invalid(format!(
                "Operator \"{}\" does not apply to \"{}\" (expected \"before\", \"after\" or \
                 \"between\").",
                other, field
            ))),
        }
    };

    match field {
        "author.operator" | "stacker.operator" => {
            let kind = if field.starts_with("author") { ActionKind::Author } else { ActionKind::Stacker };
            let (negate, values) = membership_values(object, op, field)?;
            Ok(Leaf::RecordedOperator { kind, negate, values: string_values(values, field)? })
        }

        "author.date" => date_leaf(ActionKind::Author),
        "stacker.date" => date_leaf(ActionKind::Stacker),

        "author.class" | "signer.class" => {
            let (negate, values) = membership_values(object, op, field)?;
            let values = string_values(values, field)?
                .iter()
                .map(|value| IdentityClass::parse(value))
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            Ok(Leaf::Class { negate, values })
        }

        "author.supervisor" => {
            let (negate, values) = membership_values(object, op, field)?;
            let values = values
                .into_iter()
                .map(|value| match value {
                    Value::Null => Ok(None),
                    Value::String(text) => Ok(Some(text.clone())),
                    other => Err(invalid(format!(
                        "\"author.supervisor\" compares strings or null; got {}.",
                        other
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Leaf::Supervisor { negate, values })
        }

        "author.role" => {
            let (negate, values) = membership_values(object, op, field)?;
            let values = string_values(values, field)?
                .iter()
                .map(|value| Role::parse(value))
                .collect::<Result<Vec<_>, _>>()
                .map_err(invalid)?;
            Ok(Leaf::RoleIs { negate, values })
        }

        "signer.key" => {
            let (negate, values) = membership_values(object, op, field)?;
            if negate {
                return Err(invalid("\"signer.key\" supports \"eq\" and \"in\" only.".to_string()));
            }
            Ok(Leaf::SignerKey { prefixes: string_values(values, field)? })
        }

        "signer.operator" => {
            let (negate, values) = membership_values(object, op, field)?;
            Ok(Leaf::SignerOperator { negate, values: string_values(values, field)? })
        }

        "signer.boundary" => {
            let (negate, values) = membership_values(object, op, field)?;
            if negate {
                return Err(invalid("\"signer.boundary\" supports \"eq\" and \"in\" only.".to_string()));
            }
            let values = string_values(values, field)?
                .into_iter()
                .map(|value| match value.as_str() {
                    "vouched" => Ok(Boundary::Vouched),
                    "suspect" => Ok(Boundary::Suspect),
                    other => Err(invalid(format!(
                        "\"signer.boundary\" compares \"vouched\" or \"suspect\"; got \"{}\".",
                        other
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Leaf::SignerBoundary { values })
        }

        "description" => {
            if op != "matches" {
                return Err(invalid(
                    "\"description\" supports the \"matches\" operator only.".to_string(),
                ));
            }
            let glob = object
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("\"description\" matches against a string.".to_string()))?;
            if glob.chars().count() > MAX_GLOB_CHARS {
                return Err(invalid(format!(
                    "The \"matches\" pattern is longer than the maximum of {} characters.",
                    MAX_GLOB_CHARS
                )));
            }
            Ok(Leaf::Description { glob: glob.to_string() })
        }

        "is_merge" => {
            if op != "eq" {
                return Err(invalid("\"is_merge\" supports \"eq\" only.".to_string()));
            }
            let value = object
                .get("value")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("\"is_merge\" compares a boolean.".to_string()))?;
            Ok(Leaf::IsMerge { value })
        }

        "parents.count" => {
            if op != "eq" {
                return Err(invalid("\"parents.count\" supports \"eq\" only.".to_string()));
            }
            let value = object
                .get("value")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid("\"parents.count\" compares a non-negative integer.".to_string()))?;
            Ok(Leaf::ParentsCount { value: value as usize })
        }

        "parcel" => {
            let (negate, values) = membership_values(object, op, field)?;
            if negate {
                return Err(invalid("\"parcel\" supports \"eq\" and \"in\" only.".to_string()));
            }
            Ok(Leaf::ParcelPrefix { prefixes: string_values(values, field)? })
        }

        "provenance.model" | "provenance.tool" | "provenance.session" => {
            let provenance_field = match field {
                "provenance.model" => ProvenanceField::Model,
                "provenance.tool" => ProvenanceField::Tool,
                _ => ProvenanceField::Session,
            };

            if op == "matches" {
                let glob = object
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid(format!("\"{}\" matches against a string.", field)))?;
                if glob.chars().count() > MAX_GLOB_CHARS {
                    return Err(invalid(format!(
                        "The \"matches\" pattern is longer than the maximum of {} characters.",
                        MAX_GLOB_CHARS
                    )));
                }
                return Ok(Leaf::Provenance {
                    field: provenance_field,
                    test: ProvenanceTest::Matches { glob: glob.to_string() },
                });
            }

            let (negate, values) = membership_values(object, op, field)?;
            Ok(Leaf::Provenance {
                field: provenance_field,
                test: ProvenanceTest::Membership { negate, values: string_values(values, field)? },
            })
        }

        "tag" => {
            let (negate, values) = membership_values(object, op, field)?;
            if negate {
                return Err(invalid("\"tag\" supports \"eq\" and \"in\" only.".to_string()));
            }
            Ok(Leaf::Tag { values: string_values(values, field)? })
        }

        "path" => {
            if op != "touches" {
                return Err(invalid("\"path\" supports the \"touches\" operator only.".to_string()));
            }
            let raw = object
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("\"path\" touches a warehouse-relative path string.".to_string()))?;
            // The same normalization every other path-taking command uses, so `--touches foo`
            // and `--touches ./foo/` (and the equivalent `--where` leaf) agree on one key —
            // both the CLI flag and a raw `--where` payload route through this same parser.
            let normalized = WarehousePath::from_user_input(raw)
                .map_err(|error| invalid(format!("\"path\" is not a valid warehouse path: {}", error)))?;
            Ok(Leaf::Touches { path: normalized.as_key().to_string() })
        }

        other => Err(invalid(format!(
            "Unknown query field \"{}\". Fields: author.operator, author.date, author.class, \
             author.supervisor, author.role, stacker.operator, stacker.date, signer.key, \
             signer.operator, signer.boundary, description, is_merge, parents.count, parcel, \
             provenance.model, provenance.tool, provenance.session, tag, path.",
            other
        ))),
    }
}

// ---------------------------------------------------------------------------------------------
// Matching (glob) and three-valued evaluation
// ---------------------------------------------------------------------------------------------

/// Glob (`*`, `?`) or literal-substring match, never regex. A pattern with no wildcard is a
/// substring test (the ergonomic reading of `--grep fix`); a pattern with wildcards must
/// match the whole text. Iterative with single backtrack, so cost is O(text × pattern) worst
/// case with both operands already bounded.
fn glob_match(pattern: &str, text: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return text.contains(pattern);
    }

    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;

    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }

    pattern[p..].iter().all(|&c| c == '*')
}

/// Kleene three-valued truth: an identity leaf is `Unknown` until (and unless) the verified
/// resolution exists, so phase 1 can prune only what is false regardless of identity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

impl Truth {
    fn of(value: bool) -> Truth {
        if value { Truth::True } else { Truth::False }
    }

    fn and(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::False, _) | (_, Truth::False) => Truth::False,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::True,
        }
    }

    fn or(self, other: Truth) -> Truth {
        match (self, other) {
            (Truth::True, _) | (_, Truth::True) => Truth::True,
            (Truth::Unknown, _) | (_, Truth::Unknown) => Truth::Unknown,
            _ => Truth::False,
        }
    }

    fn not(self) -> Truth {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }
}

/// The identity facts an identity leaf evaluates against: the resolved identity, or
/// `Unresolved` (phase 1 under verified trust), or `Unknowable` (resolved, but the parcel is
/// unsigned / signed by an untracked key — no forge-proof identity exists).
enum IdentityFacts<'a> {
    Unresolved,
    Unknowable,
    Resolved {
        operator: &'a str,
        class: Option<IdentityClass>,
        supervisor: Option<&'a str>,
        role: Option<Role>,
        signer_key: Option<&'a str>,

        /// The resolution's trust, so `signer.boundary` can tell a live `Verified` match
        /// (definitively not a revoked-key one) from a `SignedRevoked` one (where `boundary`
        /// below actually answers the question).
        trust: MatchTrust,

        /// Present exactly when `trust` is `SignedRevoked` (filled by
        /// [`QueryContext::fill_boundary`] before `evaluate` runs).
        boundary: Option<Boundary>,
    },
}

/// Evaluate the predicate for one parcel. `identity` supplies the identity facts at the
/// caller's chosen trust; parcel-local leaves read the parcel directly; `ctx` supplies the
/// Stage 2 ephemeral indexes and the sparse-degrade scope for `touches`.
///
/// Fallible only because `touches` may need to read a tree: every other leaf is a pure
/// lookup. A read failure here is a hard error for the whole query — same as a missing
/// parcel body elsewhere in this walk — unless [`evaluate_touches`] itself has already
/// classified it as an honest sparse degrade.
fn evaluate(
    predicate: &Predicate,
    hash: &str,
    parcel: &Parcel,
    identity: &IdentityFacts,
    ctx: &QueryContext,
) -> Result<Truth, String> {
    Ok(match predicate {
        // Kleene absorption makes stopping early sound: `False` absorbs `and` and `True`
        // absorbs `or`, so once the accumulator hits its absorbing value no later sibling can
        // move it. This matters beyond the obvious IO saving: a `touches` leaf reads a tree
        // and can bump `out_of_scope` (or hard-error on an in-scope-but-missing tree); once a
        // cheaper leaf has already decided the `all`/`any`, that leaf never runs at all. So
        // which leaves actually execute — and therefore the `out_of_scope` count and whether a
        // `touches` hard-error even surfaces — depends on predicate order once a branch is
        // decided. That is deliberate: a decided parcel owes the rest of the predicate nothing.
        Predicate::All(children) => {
            let mut acc = Truth::True;
            for child in children {
                acc = acc.and(evaluate(child, hash, parcel, identity, ctx)?);
                if acc == Truth::False {
                    break;
                }
            }
            acc
        }
        Predicate::Any(children) => {
            let mut acc = Truth::False;
            for child in children {
                acc = acc.or(evaluate(child, hash, parcel, identity, ctx)?);
                if acc == Truth::True {
                    break;
                }
            }
            acc
        }
        Predicate::Not(child) => evaluate(child, hash, parcel, identity, ctx)?.not(),
        Predicate::Leaf(leaf) => evaluate_leaf(leaf, hash, parcel, identity, ctx)?,
    })
}

/// Actions of one kind, as (operator, timestamp) pairs.
fn actions_of(parcel: &Parcel, kind: ActionKind) -> impl Iterator<Item = (&str, i64)> {
    parcel.actions.iter().filter_map(move |action| {
        let matches_kind = match kind {
            ActionKind::Author => matches!(action.action, ParcelActionType::Author),
            ActionKind::Stacker => matches!(action.action, ParcelActionType::Stack),
        };
        matches_kind.then(|| (action.operator.identifier.as_str(), action.timestamp.timestamp()))
    })
}

fn evaluate_leaf(
    leaf: &Leaf,
    hash: &str,
    parcel: &Parcel,
    identity: &IdentityFacts,
    ctx: &QueryContext,
) -> Result<Truth, String> {
    // Identity leaves resolve against the identity facts; everything else is parcel-local.
    if leaf.is_identity() {
        let (operator, class, supervisor, role, signer_key, trust, boundary) = match identity {
            IdentityFacts::Unresolved => return Ok(Truth::Unknown),
            // No forge-proof identity exists: an identity test neither matches nor
            // negation-matches — three-valued honesty, not silent exclusion or inclusion.
            IdentityFacts::Unknowable => return Ok(Truth::Unknown),
            IdentityFacts::Resolved { operator, class, supervisor, role, signer_key, trust, boundary } => {
                (*operator, *class, *supervisor, *role, *signer_key, *trust, *boundary)
            }
        };

        return Ok(match leaf {
            Leaf::Class { negate, values } => match class {
                Some(class) => Truth::of(values.contains(&class) != *negate),
                // Resolved to an operator the office does not enroll: the class is
                // unknowable, not "human by default" — a compliance filter must not guess.
                None => Truth::Unknown,
            },
            Leaf::Supervisor { negate, values } => {
                let supervisor = supervisor.map(str::to_string);
                Truth::of(values.contains(&supervisor) != *negate)
            }
            Leaf::RoleIs { negate, values } => match role {
                Some(role) => Truth::of(values.contains(&role) != *negate),
                None => Truth::Unknown,
            },
            Leaf::SignerKey { prefixes } => match signer_key {
                Some(key) => Truth::of(prefixes.iter().any(|prefix| key.starts_with(prefix))),
                None => Truth::Unknown,
            },
            Leaf::SignerOperator { negate, values } => {
                Truth::of(values.iter().any(|value| value == operator) != *negate)
            }
            // A `SignedRevoked` match compares against the boundary `QueryContext::fill_boundary`
            // filled before this call; a live `Verified` match is definitively not a revoked-key
            // one (`False`, not `Unknown` — the signer facts are fully resolved either way);
            // anything else identity-resolved here (only `Recorded`, which never reaches this
            // leaf — signer predicates are refused outright under recorded trust) is honestly
            // unknowable.
            Leaf::SignerBoundary { values } => match trust {
                MatchTrust::SignedRevoked => Truth::of(boundary.is_some_and(|boundary| values.contains(&boundary))),
                MatchTrust::Verified => Truth::False,
                _ => Truth::Unknown,
            },
            _ => unreachable!("is_identity() covers exactly the identity leaves"),
        });
    }

    if let Leaf::Touches { path } = leaf {
        return evaluate_touches(parcel, path, ctx);
    }

    Ok(match leaf {
        Leaf::RecordedOperator { kind, negate, values } => {
            let any = actions_of(parcel, *kind).any(|(operator, _)| {
                values.iter().any(|value| value == operator)
            });
            Truth::of(any != *negate)
        }

        Leaf::Date { kind, after, before } => {
            let any = actions_of(parcel, *kind).any(|(_, ts)| {
                after.is_none_or(|bound| ts > bound) && before.is_none_or(|bound| ts < bound)
            });
            Truth::of(any)
        }

        Leaf::Description { glob } => {
            let in_parcel = parcel
                .description
                .as_deref()
                .is_some_and(|description| glob_match(glob, description));
            let in_actions = parcel.actions.iter().any(|action| {
                action.description.as_deref().is_some_and(|description| glob_match(glob, description))
            });
            Truth::of(in_parcel || in_actions)
        }

        Leaf::IsMerge { value } => Truth::of((parcel.parents.len() > 1) == *value),

        Leaf::ParentsCount { value } => Truth::of(parcel.parents.len() == *value),

        Leaf::ParcelPrefix { prefixes } => {
            Truth::of(prefixes.iter().any(|prefix| hash.starts_with(prefix)))
        }

        // A5: provenance is opt-in evidence. No `@manifest` head at all, or no entry for
        // this subject, both read `Unknown` — never `False` — so `not: {provenance.* …}`
        // can never quietly sweep up a parcel that carries no provenance whatsoever.
        Leaf::Provenance { field, test } => match ctx.provenance.as_ref().and_then(|index| index.get(hash)) {
            None => Truth::Unknown,
            Some(records) => match test {
                // The aggregate-then-negate convention every other membership leaf in this
                // file uses: `ne` reads "no entry equals this", not "some entry doesn't" —
                // the same shape as `SignerOperator`/`RecordedOperator`.
                ProvenanceTest::Membership { negate, values } => {
                    let any = records.iter()
                        .any(|record| values.iter().any(|v| v == provenance_field_value(*field, record)));
                    Truth::of(any != *negate)
                }
                ProvenanceTest::Matches { glob } => Truth::of(
                    records.iter().any(|record| glob_match(glob, provenance_field_value(*field, record))),
                ),
            },
        },

        // Membership, not evidence: an untagged parcel plainly does not carry the tag
        // (`False`); only the whole `@tags` pallet being unreadable is `Unknown`.
        Leaf::Tag { values } => match &ctx.tags {
            None => Truth::Unknown,
            Some(index) => {
                let tags = index.get(hash).map(Vec::as_slice).unwrap_or(&[]);
                Truth::of(values.iter().any(|value| tags.contains(value)))
            }
        },

        // `Touches` is handled above (it may need to read a tree, hence the early return);
        // it can never reach this match.
        _ => unreachable!("identity leaves are handled above; touches is handled before this match"),
    })
}

/// The string a `provenance.*` leaf compares, for one entry.
fn provenance_field_value(field: ProvenanceField, record: &ProvenanceRecord) -> &str {
    match field {
        ProvenanceField::Model => &record.model,
        ProvenanceField::Tool => record.tool.as_deref().unwrap_or(""),
        ProvenanceField::Session => record.session.as_deref().unwrap_or(""),
    }
}

/// `path` / `touches`: whether the parcel's tree differs from its **first parent's** at
/// `path` — the exact diff `blame` already does for its per-line attribution, so a merge
/// parcel is judged by the same rule blame documents as its "honest limit of a first-parent
/// walk": whichever parcel's *own* tree actually differs from what its first parent already
/// had gets the credit. A merge that pulls in a change its first parent altogether lacked
/// reads as touching the path too (same as blame attributing those lines to the merge, not
/// the side branch) — but a merge whose result at the path happens to coincide with what its
/// first parent already had (both sides added the identical content, or the resolution kept
/// the first-parent side) reads as untouched there, even though the other branch touched it
/// on its own line. In short: `touches` never looks past a parcel's first parent, so credit
/// always lands on whichever step actually changed the tree relative to it, not on "did the
/// change enter history somewhere upstream."
///
/// This does **not** gate on the commit-graph's changed-path Bloom filter the way `blame`
/// does, despite the filter looking like a free win here too. `graph_utils::path_maybe_changed`
/// only ever records *leaf file* paths (`compute_filter`'s tree diff inserts a changed file's
/// full path, never any of its ancestor directories) — exactly enough for `blame`, which only
/// ever probes a single file path. A `touches` predicate's path is routinely a directory
/// prefix (`--touches src`), and probing the filter for the literal string `"src"` would almost
/// always read `false` even when a file changed underneath it — a false negative the "no false
/// negatives" guarantee this predicate depends on cannot absorb. So every candidate gets the
/// real confirming diff below; the Bloom filter stays exactly what `blame` needs, unmodified
/// (extending it to also index ancestor prefixes would fix this cheaply, but would also need a
/// migration for whatever a warehouse already has computed and stored on disk — a bigger change
/// than this predicate warrants).
fn evaluate_touches(parcel: &Parcel, path: &str, ctx: &QueryContext) -> Result<Truth, String> {
    match confirm_touched(parcel, path, ctx) {
        Ok(changed) => Ok(Truth::of(changed)),
        Err(error) => {
            // Finding 3: the confirming diff needs a tree a sparse warehouse never fetched.
            // Definitive only when the path is provably outside the fetch scope — a tree
            // missing that the scope says *should* be present is tampering, not sparseness,
            // and stays a hard error exactly like any other object read in this walk.
            if ctx.fetch_scope.classify(path) == ScopeClass::OutOfScope {
                ctx.out_of_scope.set(ctx.out_of_scope.get() + 1);
                Ok(Truth::Unknown)
            } else {
                Err(error)
            }
        }
    }
}

/// The confirming diff `touches` needs once the Bloom filter says "maybe": resolve `path` in
/// the parcel's tree and in its first parent's (a root parcel's "first parent" is the absent
/// empty tree — every path reads as not-present there), and compare entry hashes. Any
/// difference, including one side having no entry at all, is a change.
///
/// The first parent's tree hash comes from `ctx`'s memo, not a fresh `load_parcel`: the walk
/// in `run_query` already loads (and decodes) every parent's full body once, to enqueue it,
/// and parcels deliberately bypass the shared read cache — a second uncached load per
/// evaluation would double that cost across nearly every walked parcel. The memo only misses
/// on a parent the walk never itself visited (already dropped from a previous page's
/// frontier, or a seed).
fn confirm_touched(parcel: &Parcel, path: &str, ctx: &QueryContext) -> Result<bool, String> {
    let new_hash = resolve_entry_hash(&parcel.tree_hash, path)?;
    let old_hash = match parcel.parents.first() {
        Some(parent) => resolve_entry_hash(&ctx.tree_hash_of(parent)?, path)?,
        None => None,
    };

    Ok(new_hash != old_hash)
}

/// Resolve a warehouse path to whatever sits there — file or directory — as its entry hash,
/// or `None` when nothing does. A directory's tree hash already encodes everything beneath
/// it, so comparing that one hash is the same "identical subtree, nothing changed under it"
/// shortcut the commit-graph's own Bloom-filter builder (`graph_utils::compute_filter`) relies
/// on internally.
fn resolve_entry_hash(tree_hash: &str, path: &str) -> Result<Option<String>, String> {
    if let Some((hash, _)) = object_utils::resolve_tree_file(tree_hash, path)? {
        return Ok(Some(hash));
    }
    tree_utils::resolve_subtree_hash(tree_hash, path)
}

// ---------------------------------------------------------------------------------------------
// Identity resolution
// ---------------------------------------------------------------------------------------------

/// Resolve one parcel's verified identity: the real signature classification, then the
/// office join off the *verified signer* — never off the parcel's self-declared operator.
///
/// Signature-only classification: the walk already loaded (and thereby presence-proved)
/// every parcel it hands here, and parcels bypass the shared read cache, so the
/// body-re-reading variant would double parcel IO for nothing.
fn resolve_verified(hash: &str, office: &OfficeState) -> Result<IdentityResolution, String> {
    let trust = audit_utils::classify_signature_trust(hash, office)?;

    let (match_trust, key_id) = match trust {
        SignatureTrust::Verified { key_id } => (MatchTrust::Verified, Some(key_id)),
        SignatureTrust::SignedRevoked { key_id } => (MatchTrust::SignedRevoked, Some(key_id)),
        SignatureTrust::Unsigned => (MatchTrust::Unsigned, None),
        // The claimed key id is attacker-writable sidecar content that verified nothing;
        // it is deliberately not reported as a signer key.
        SignatureTrust::UnknownKey { .. } => (MatchTrust::UnknownKey, None),
    };

    let key = key_id.as_deref().and_then(|key_id| office.find_key(key_id));
    let operator = key.map(|key| key.operator.clone());
    let user = operator.as_deref().and_then(|operator| office.find_user(operator));

    Ok(IdentityResolution {
        trust: match_trust,
        class: user.map(|user| user.class),
        supervisor: user.and_then(|user| user.supervisor.clone()),
        role: user.map(|user| user.role),
        revocation_reason: key.and_then(|key| key.revocation_reason),
        signer_key: key_id,
        operator,
        // Filled by `QueryContext::fill_boundary` on the main walk thread, not here: this
        // runs inside `fanout_utils::fanout_map`'s worker threads, where the boundary memo
        // must not need a `Mutex`.
        boundary: None,
    })
}

/// Resolve one parcel's recorded identity: the first authoring action's self-declared
/// operator, joined against the office. Forgeable by construction; labeled `recorded`.
fn resolve_recorded(parcel: &Parcel, office: &OfficeState) -> IdentityResolution {
    let operator = actions_of(parcel, ActionKind::Author)
        .map(|(operator, _)| operator)
        .next()
        .or_else(|| parcel.actions.first().map(|action| action.operator.identifier.as_str()))
        .map(str::to_string);

    let user = operator.as_deref().and_then(|operator| office.find_user(operator));

    IdentityResolution {
        trust: MatchTrust::Recorded,
        class: user.map(|user| user.class),
        supervisor: user.and_then(|user| user.supervisor.clone()),
        role: user.map(|user| user.role),
        signer_key: None,
        revocation_reason: None,
        operator,
        // Recorded trust never resolves a signer, so a boundary question never arises.
        boundary: None,
    }
}

fn identity_facts<'a>(resolution: &'a IdentityResolution) -> IdentityFacts<'a> {
    match resolution.operator.as_deref() {
        Some(operator) => IdentityFacts::Resolved {
            operator,
            class: resolution.class,
            supervisor: resolution.supervisor.as_deref(),
            role: resolution.role,
            signer_key: resolution.signer_key.as_deref(),
            trust: resolution.trust,
            boundary: resolution.boundary,
        },
        None => IdentityFacts::Unknowable,
    }
}

// ---------------------------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------------------------

/// Below this many pending phase-2 resolutions the Ed25519 verifies are cheaper than the
/// threads that would share them (the audit's own threshold); this is also the batch size,
/// so a batch is exactly what crosses into profitable fan-out.
const PHASE2_BATCH: usize = 256;

/// Run a query: walk from the seeds newest-first, filter with the two-phase trust ordering,
/// and hand each match to `on_match` (return `false` to stop early — the reader went away).
/// Matching parcels stream in walk order; the outcome carries the resume cursor.
///
/// A parcel that cannot be loaded — including a parcel body missing from the store — errors
/// the whole query: the parcel spine is never sparse, so a gap there is an incomplete fetch
/// or tampering, and a compliance answer must not paper over it.
pub fn run_query(
    params: &QueryParams,
    office: &OfficeState,
    mut on_match: impl FnMut(QueryMatch) -> bool,
) -> Result<QueryOutcome, String> {
    if params.trust == TrustMode::Recorded && params.predicate.has_signer_leaves() {
        return Err(invalid(
            "Signer predicates (signer.key, signer.operator) answer only under verified \
             trust; drop --recorded or the signer predicate.",
        ));
    }

    // The Stage 2 ephemeral indexes (provenance, tags, fetch scope) are built once, always —
    // even when the predicate never reads them — because they enrich every reported match's
    // report row (`provenance`/`tags`), not only the parcels a predicate actually filters on.
    let ctx = QueryContext::build()?;

    let mut heap: BinaryHeap<(i64, String)> = BinaryHeap::new();
    let mut loaded: HashMap<String, Parcel> = HashMap::new();
    let mut enqueued: HashSet<String> = HashSet::new();

    for seed in &params.seeds {
        if enqueued.insert(seed.clone()) {
            let parcel = object_utils::load_parcel(seed)?;
            heap.push((latest_action_timestamp(&parcel), seed.clone()));
            ctx.remember_tree_hash(seed, &parcel.tree_hash);
            loaded.insert(seed.clone(), parcel);
        }
    }

    let identity_filtering = params.predicate.has_identity_leaves();
    let verified = params.trust == TrustMode::Verified;

    let mut walked = 0usize;
    let mut matched = 0usize;
    // Phase-1 survivors awaiting phase-2 resolution (verified trust with identity
    // predicates): resolved in batches so the Ed25519 work fans out across cores.
    let mut pending: Vec<(String, Parcel)> = Vec::new();

    // The two ways the walk ends: cleanly (history exhausted / reader gone → no cursor), or
    // at the limit (cursor = the sorted frontier plus any still-undecided survivors).
    macro_rules! outcome_at_limit {
        ($leftover:expr) => {{
            let mut frontier: Vec<String> =
                heap.into_iter().map(|(_, hash)| hash).collect();
            frontier.extend($leftover);
            frontier.sort();
            frontier.dedup();
            return Ok(QueryOutcome {
                next: (!frontier.is_empty()).then(|| frontier.join(",")),
                walked,
                matched,
                out_of_scope: ctx.out_of_scope.get(),
                provenance_present: ctx.provenance.is_some(),
                tags_present: ctx.tags.is_some(),
            });
        }};
    }

    while let Some((_, hash)) = heap.pop() {
        let parcel = loaded.remove(&hash).expect("every heap entry has its parcel loaded");

        // `--from` scope: the excluded parcel and its ancestors never enter the answer, and
        // a walk already inside the excluded cone stops descending (every parent of an
        // excluded parcel is itself an ancestor of `from`).
        if let Some(from) = &params.from {
            if hash == *from || merge_utils::is_ancestor(&hash, from)? {
                continue;
            }
        }

        walked += 1;

        // Parents enqueue before any limit stop (so the frontier cursor stays complete) and
        // — moved ahead of `evaluate` below — before phase 1 runs, too: `touches` needs the
        // first parent's tree hash, and loading (and memoizing) it here means `confirm_touched`
        // finds it already in `ctx`'s memo instead of paying for a second uncached parcel load.
        for parent in &parcel.parents {
            if enqueued.insert(parent.clone()) {
                let parent_parcel = object_utils::load_parcel(parent)?;
                heap.push((latest_action_timestamp(&parent_parcel), parent.clone()));
                ctx.remember_tree_hash(parent, &parent_parcel.tree_hash);
                loaded.insert(parent.clone(), parent_parcel);
            }
        }

        // Phase 1: prune on what needs no identity. An identity leaf reads Unknown (or, under
        // recorded trust, the recorded facts — pruning early there is the mode's stated point).
        let recorded_resolution =
            (!verified).then(|| resolve_recorded(&parcel, office));
        let phase1_facts = match &recorded_resolution {
            Some(resolution) => identity_facts(resolution),
            None => IdentityFacts::Unresolved,
        };
        let phase1 = evaluate(&params.predicate, &hash, &parcel, &phase1_facts, &ctx)?;

        if phase1 == Truth::False {
            continue;
        }

        if !verified {
            // Recorded trust: phase 1 is the whole decision — and only a definite match is
            // one. (Unknown here means e.g. a class test against an operator the office
            // does not enroll: the answer is unknowable, and a compliance filter must not
            // guess, in either direction.)
            if phase1 != Truth::True {
                continue;
            }
            let resolution = recorded_resolution.expect("recorded resolution built above");
            matched += 1;
            let provenance = ctx.newest_provenance(&hash).cloned();
            let tags = ctx.tags_of(&hash).to_vec();
            if !on_match(QueryMatch { hash, parcel, identity: resolution, provenance, tags }) {
                return Ok(QueryOutcome {
                    next: None, walked, matched,
                    out_of_scope: ctx.out_of_scope.get(),
                    provenance_present: ctx.provenance.is_some(),
                    tags_present: ctx.tags.is_some(),
                });
            }
            if params.limit.is_some_and(|limit| matched >= limit) {
                outcome_at_limit!(Vec::new());
            }
            continue;
        }

        if !identity_filtering {
            // Verified trust, but no identity predicate: previously this meant phase 1 alone
            // was always definite (`Description`/`IsMerge`/… never produce `Unknown`). Stage
            // 2's non-identity leaves break that: a provenance/tag leaf with no evidence for
            // this parcel, or a `touches` leaf whose confirming diff fell outside a sparse
            // fetch scope, can read `Unknown` here too. `Unknown` never matches — the same
            // "excluded, not guessed" rule identity leaves already followed under recorded
            // trust, applied uniformly rather than assumed away.
            if phase1 != Truth::True {
                continue;
            }
            let mut resolution = resolve_verified(&hash, office)?;
            ctx.fill_boundary(&hash, &mut resolution, office)?;
            matched += 1;
            let provenance = ctx.newest_provenance(&hash).cloned();
            let tags = ctx.tags_of(&hash).to_vec();
            if !on_match(QueryMatch { hash, parcel, identity: resolution, provenance, tags }) {
                return Ok(QueryOutcome {
                    next: None, walked, matched,
                    out_of_scope: ctx.out_of_scope.get(),
                    provenance_present: ctx.provenance.is_some(),
                    tags_present: ctx.tags.is_some(),
                });
            }
            if params.limit.is_some_and(|limit| matched >= limit) {
                outcome_at_limit!(Vec::new());
            }
            continue;
        }

        // Verified trust with identity predicates: every phase-1 survivor gets resolved —
        // never a subset chosen by the recorded value being verified.
        pending.push((hash, parcel));

        if pending.len() >= PHASE2_BATCH {
            let survivors = std::mem::take(&mut pending);
            let resolutions = resolve_batch(&survivors, office)?;
            let mut remaining = survivors.into_iter().zip(resolutions);

            while let Some(((hash, parcel), mut resolution)) = remaining.next() {
                ctx.fill_boundary(&hash, &mut resolution, office)?;
                let verdict =
                    evaluate(&params.predicate, &hash, &parcel, &identity_facts(&resolution), &ctx)?;
                if verdict != Truth::True {
                    continue;
                }
                matched += 1;
                let provenance = ctx.newest_provenance(&hash).cloned();
                let tags = ctx.tags_of(&hash).to_vec();
                if !on_match(QueryMatch { hash, parcel, identity: resolution, provenance, tags }) {
                    return Ok(QueryOutcome {
                        next: None, walked, matched,
                        out_of_scope: ctx.out_of_scope.get(),
                        provenance_present: ctx.provenance.is_some(),
                        tags_present: ctx.tags.is_some(),
                    });
                }
                if params.limit.is_some_and(|limit| matched >= limit) {
                    // Batch members after this one are popped but undecided: they resume
                    // as next page's seeds (cheap to re-evaluate, never lost).
                    let leftover: Vec<String> = remaining.map(|((hash, _), _)| hash).collect();
                    outcome_at_limit!(leftover);
                }
            }
        }
    }

    // The walk is exhausted; decide the survivors still pending.
    let survivors = std::mem::take(&mut pending);
    let resolutions = resolve_batch(&survivors, office)?;
    let mut remaining = survivors.into_iter().zip(resolutions);

    while let Some(((hash, parcel), mut resolution)) = remaining.next() {
        ctx.fill_boundary(&hash, &mut resolution, office)?;
        let verdict = evaluate(&params.predicate, &hash, &parcel, &identity_facts(&resolution), &ctx)?;
        if verdict != Truth::True {
            continue;
        }
        matched += 1;
        let provenance = ctx.newest_provenance(&hash).cloned();
        let tags = ctx.tags_of(&hash).to_vec();
        if !on_match(QueryMatch { hash, parcel, identity: resolution, provenance, tags }) {
            return Ok(QueryOutcome {
                next: None, walked, matched,
                out_of_scope: ctx.out_of_scope.get(),
                provenance_present: ctx.provenance.is_some(),
                tags_present: ctx.tags.is_some(),
            });
        }
        if params.limit.is_some_and(|limit| matched >= limit) {
            let leftover: Vec<String> = remaining.map(|((hash, _), _)| hash).collect();
            outcome_at_limit!(leftover);
        }
    }

    Ok(QueryOutcome {
        next: None, walked, matched,
        out_of_scope: ctx.out_of_scope.get(),
        provenance_present: ctx.provenance.is_some(),
        tags_present: ctx.tags.is_some(),
    })
}

/// Resolve a batch of survivors' verified identities, fanning out across the cores once the
/// batch is big enough that the Ed25519 verifies outweigh the threads (the audit idiom).
fn resolve_batch(
    survivors: &[(String, Parcel)],
    office: &OfficeState,
) -> Result<Vec<IdentityResolution>, String> {
    if survivors.len() < PHASE2_BATCH {
        return survivors
            .iter()
            .map(|(hash, _)| resolve_verified(hash, office))
            .collect();
    }

    let hashes: Vec<String> = survivors.iter().map(|(hash, _)| hash.clone()).collect();
    fanout_utils::fanout_map(&hashes, |hash| resolve_verified(hash, office))
        .into_iter()
        .collect()
}

/// The latest action timestamp of a parcel (Unix seconds) — the walk's newest-first order,
/// same as history's (recorded, honesty caveat and all).
fn latest_action_timestamp(parcel: &Parcel) -> i64 {
    parcel
        .actions
        .iter()
        .map(|action| action.timestamp.timestamp())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(field: &str, op: &str, value: Value) -> Value {
        serde_json::json!({ "field": field, "op": op, "value": value })
    }

    #[test]
    fn globs_match_wildcards_and_bare_patterns_match_substrings() {
        assert!(glob_match("claude-*", "claude-opus-4"));
        assert!(!glob_match("claude-*x", "claude-opus-4"));
        assert!(glob_match("c?aude*4", "claude-opus-4"));
        assert!(glob_match("fix", "a fix for the walk"));
        assert!(!glob_match("fix", "nothing here"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("?", ""));
    }

    #[test]
    fn depth_leaves_in_and_glob_bounds_all_refuse() {
        // Depth: nest `not` past the limit.
        let mut nested = leaf("is_merge", "eq", Value::Bool(true));
        for _ in 0..MAX_PREDICATE_DEPTH {
            nested = serde_json::json!({ "not": nested });
        }
        assert!(parse_value(&nested).is_err());

        // Leaves: one more than the limit.
        let leaves: Vec<Value> =
            (0..=MAX_PREDICATE_LEAVES).map(|_| leaf("is_merge", "eq", Value::Bool(true))).collect();
        assert!(parse_value(&serde_json::json!({ "all": leaves })).is_err());

        // `in` array length.
        let over: Vec<Value> = (0..=MAX_IN_VALUES).map(|i| Value::String(i.to_string())).collect();
        assert!(parse_value(&leaf("author.operator", "in", Value::Array(over))).is_err());

        // Glob length.
        let long = "x".repeat(MAX_GLOB_CHARS + 1);
        assert!(parse_value(&leaf("description", "matches", Value::String(long))).is_err());

        // Payload bytes.
        let padding = "x".repeat(MAX_WHERE_BYTES);
        assert!(parse_where(&padding).is_err());
    }

    #[test]
    fn unknown_fields_ops_and_bad_json_refuse_with_the_predicate_code() {
        for payload in [
            // `provenance.model` was this probe's unknown-field case before Stage 2 made it
            // a real field; `bogus.field` keeps testing the same thing (an unrecognized
            // field name refuses).
            "{\"field\": \"bogus.field\", \"op\": \"eq\", \"value\": \"x\"}",
            "{\"field\": \"is_merge\", \"op\": \"matches\", \"value\": \"x\"}",
            "not json at all",
            "{\"all\": [], \"any\": []}",
        ] {
            let error = match parse_where(payload) {
                Err(error) => error,
                Ok(_) => panic!("payload {:?} unexpectedly parsed", payload),
            };
            let core: CoreError = error.into();
            match core {
                CoreError::Refusal { code, .. } => {
                    assert_eq!(code, RefusalCode::QueryPredicateInvalid)
                }
                other => panic!("expected a predicate refusal, got {:?}", other),
            }
        }
    }

    #[test]
    fn identity_leaves_are_unknown_in_phase_one_and_kleene_logic_holds() {
        let predicate = parse_value(&serde_json::json!({
            "not": leaf("author.class", "eq", Value::String("agent".to_string()))
        }))
        .unwrap();

        let parcel = Parcel {
            tree_hash: String::new(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: None,
        };

        // A context with no provenance/tags evidence and a full fetch scope — this test is
        // only exercising the identity Kleene logic, not Stage 2's indexes.
        let ctx = QueryContext {
            provenance: None,
            tags: None,
            fetch_scope: MaterializationScope::full(),
            out_of_scope: Cell::new(0),
            tree_hashes: RefCell::new(HashMap::new()),
            boundaries: RefCell::new(HashMap::new()),
        };

        // Unresolved identity: not(Unknown) stays Unknown — the parcel survives to phase 2
        // rather than being pruned on a value nobody verified.
        let verdict = evaluate(&predicate, "abc", &parcel, &IdentityFacts::Unresolved, &ctx).unwrap();
        assert!(verdict == Truth::Unknown);

        // An unsigned parcel resolves to Unknowable: still Unknown, so an identity query
        // neither matches nor negation-matches it.
        let verdict = evaluate(&predicate, "abc", &parcel, &IdentityFacts::Unknowable, &ctx).unwrap();
        assert!(verdict == Truth::Unknown);
    }

    #[test]
    fn all_short_circuits_on_a_definitive_false_before_a_later_touches_leaf_runs() {
        // `is_merge eq true` is definitively False for a non-merge parcel; `all` must stop
        // there rather than go on to evaluate the `touches` leaf too. If it did not, `touches`
        // would try to read a tree from a storage root this test never sets up and return an
        // `Err`, which `evaluate` would propagate — so a broken short-circuit fails this test
        // loudly (via the `expect` below) rather than silently.
        let predicate = parse_value(&serde_json::json!({
            "all": [
                leaf("is_merge", "eq", Value::Bool(true)),
                leaf("path", "touches", Value::String("x".to_string())),
            ]
        }))
        .unwrap();

        let parcel = Parcel {
            tree_hash: "0".repeat(64),
            parents: Vec::new(),
            actions: Vec::new(),
            description: None,
        };

        let ctx = QueryContext {
            provenance: None,
            tags: None,
            fetch_scope: MaterializationScope::full(),
            out_of_scope: Cell::new(0),
            tree_hashes: RefCell::new(HashMap::new()),
            boundaries: RefCell::new(HashMap::new()),
        };

        let verdict = evaluate(&predicate, "abc", &parcel, &IdentityFacts::Unresolved, &ctx)
            .expect("the touches leaf must never run once `all`'s first child is already False");
        assert!(verdict == Truth::False);
        // A `touches` leaf that actually ran (with this full fetch scope) would have hard-
        // errored above rather than degrade — so a live `Ok` already proves it did not run;
        // this is a second, cheap witness of the same fact.
        assert_eq!(ctx.out_of_scope.get(), 0, "touches must not have run at all");
    }
}
