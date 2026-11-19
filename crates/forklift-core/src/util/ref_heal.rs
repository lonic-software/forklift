//! Rebuilding a lost or damaged user pallet ref from what the warehouse still says about it, for
//! `forklift heal` (DESIGN.html §3.1.1).
//!
//! ## What counts as damaged
//!
//! Two kinds of user ref, and nothing else: a ref file whose bytes the reader refuses (it does not
//! hold a parcel hash), and a missing ref that some checkout still stands on with a base. A lost
//! ref no checkout stands on cannot be told from a pallet that never existed, so it is not found.
//! Meta refs are judged by [`repair_meta`], from their copies.
//!
//! ## The evidence and the choice
//!
//! Every checkout (the warehouse's own working tree and each bay) is asked for its candidates:
//! the base of its position on the pallet, the after-head of its latest journal entry naming the
//! pallet, and an interrupted operation's starting base, plus a merge or pick's completing parcel
//! unless the marker is rolling back. A damaged file adds what its own bytes still name: the full
//! hash when it survives, the one stored parcel a hex prefix of at least 8 characters matches, or
//! the one parcel that matches at every position but one.
//!
//! The ref is rebuilt at the one candidate whose closure verifies, that equals or descends from
//! every other verified candidate, and that descends from every floor: the bases, the marker
//! heads and the salvage, which are what a checkout or the ref's own bytes stand on. A journal
//! head is not a floor, since a later rewind or a collection may have left it behind. A journal
//! head that still verifies and lies above every other candidate does win, although a rewind in
//! another checkout may have moved past it: journals are not ordered across checkouts, and of the
//! two wrong answers, a rebuild above a rewind loses nothing (the rewind can be run again) while
//! one below lost work would. An answer that cannot be read (an ancestry walk that fails, a
//! salvage match that is tainted, unreadable or one of several parcels) is unknown, never "no".
//!
//! A latest journal entry that deleted the pallet blocks the rebuild, unless a checkout stands on
//! the pallet: the pallet was then made again after the delete (`pallet create` leaves no journal
//! entry), or a shift raced the delete. Either way the rebuild keeps what that checkout stands
//! on, and the delete can be run again.
//!
//! ## Two passes
//!
//! [`Pass::BeforeTaint`] runs before the taint recovery and rebuilds only what is settled: it
//! defers while a journal head does not verify or an answer is unknown, since the recovery may yet
//! refetch what they lack. [`Pass::AfterTaint`] re-judges what is left, dropping a journal head
//! that still does not verify, and refuses with every candidate it found whatever it still cannot
//! settle.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::Serialize;

use crate::globals::forklift_root;
use crate::util::operation_utils::OperationKind;
use crate::util::pallet_utils::{self, PalletNamespace, RefDamage, RefUpdateError};
use crate::util::{bay_utils, file_utils, journal_utils, merge_utils, object_utils, operation_utils, taint_utils};

/// The shortest hex prefix a damaged ref's bytes are salvaged from.
const MIN_SALVAGE_PREFIX: usize = 8;

/// The length of a full object hash in hex.
const HASH_LENGTH: usize = 64;

/// A user pallet ref found damaged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamagedRef {
    /// The pallet.
    pub pallet: String,

    /// How its ref is damaged.
    pub damage: RefDamage,
}

/// Which of the two repair passes runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// Before the taint recovery: rebuild only what nothing still missing could change.
    BeforeTaint,

    /// After it: rebuild what can be, refuse the rest.
    AfterTaint,
}

/// A head the evidence names, and where it was found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
pub struct Candidate {
    /// The parcel.
    pub head: String,

    /// Where it was found, in words.
    pub source: String,
}

/// A ref this run rebuilt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
pub struct Rebuilt {
    /// The pallet.
    pub pallet: String,

    /// The head its ref holds now.
    pub head: String,

    /// Where that head was found, in words.
    pub source: String,
}

/// A damaged ref this run left alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
pub struct Refused {
    /// The pallet.
    pub pallet: String,

    /// Why no head was chosen.
    pub reason: String,

    /// Every head the evidence named.
    pub candidates: Vec<Candidate>,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repair {
    /// The refs it rebuilt.
    pub rebuilt: Vec<Rebuilt>,

    /// The refs it refused to rebuild (only [`Pass::AfterTaint`] refuses; the first pass defers).
    pub refused: Vec<Refused>,

    /// What narrowed the evidence: a position, journal or marker that could not be read.
    pub notes: Vec<String>,

    /// Why the pass stopped short, perhaps after writing: what it rebuilt before stopping is in
    /// `rebuilt` all the same.
    pub stopped: Option<String>,
}

/// Where a candidate came from.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// The base a checkout stands on.
    Base(String),

    /// The after-head of a checkout's latest journal entry naming the pallet.
    Journal(String),

    /// The base an interrupted operation in a checkout started from.
    Started(String, OperationKind),

    /// The completing parcel an interrupted merge or pick in a checkout recorded.
    Completing(String, OperationKind),

    /// The ref's own bytes.
    Salvage,
}

impl Source {
    fn describe(&self) -> String {
        match self {
            Source::Base(which) => format!("the base of {}", which),
            Source::Journal(which) => format!("the latest journal entry of {}", which),
            Source::Started(which, kind) => format!("the start of the interrupted {} in {}", kind.as_str(), which),
            Source::Completing(which, kind) => format!("the parcel the interrupted {} in {} completes with", kind.as_str(), which),
            Source::Salvage => "what the ref's own bytes still name".to_string(),
        }
    }

    /// A floor is a head the rebuild may not land below: what a checkout or the ref's own bytes
    /// stand on. A journal head is not one.
    fn is_floor(&self) -> bool {
        !matches!(self, Source::Journal(_))
    }
}

/// What a damaged file's own bytes still name.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Salvage {
    /// Nothing.
    Nothing,

    /// This one parcel.
    Found(String),

    /// More than one parcel: the bytes name one of these, not which. The flag says another match
    /// was tainted or could not be read, so may be the one.
    Ambiguous(Vec<String>, bool),

    /// A match whose kind cannot be read, or that is tainted.
    Unknown(String),
}

/// Everything the warehouse says about one pallet's head.
#[derive(Debug, Default)]
struct Evidence {
    candidates: Vec<(String, Source)>,

    /// The checkouts whose latest journal entry naming the pallet deleted it.
    deleted_in: Vec<String>,

    /// Why the salvage is unknown, if it is.
    salvage_unknown: Option<String>,

    /// The parcels an ambiguous salvage may name.
    salvage_ambiguous: Vec<String>,

    /// Whether an ambiguous salvage also matched an object that is tainted or cannot be read.
    salvage_dropped: bool,

    /// Whether any checkout stands on the pallet.
    stood_on: bool,

    /// Whether a checkout's position, journal or marker could not be read.
    narrowed: bool,
}

/// The checkouts whose state is read: the warehouse's own working tree, then every bay. A bays
/// folder that cannot be listed narrows this to the working tree, with a note.
fn checkouts(notes: &mut Vec<String>) -> (Vec<(String, PathBuf)>, bool) {
    let mut checkouts = vec![("the warehouse's own working tree".to_string(), forklift_root())];

    match bay_utils::list_bays() {
        Ok(bays) => {
            checkouts.extend(bays.into_iter().map(|name| (format!("bay \"{}\"", name), bay_utils::bay_state_dir(&name))));
            (checkouts, false)
        }
        Err(error) => {
            push_note(notes, format!("The bays could not be listed, so only the warehouse's own working tree was asked about damaged pallet refs: {}", error));
            (checkouts, true)
        }
    }
}

fn push_note(notes: &mut Vec<String>, note: String) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}

/// Find every damaged user pallet ref.
///
/// # Returns
/// * `Ok(Vec<DamagedRef>)` - The damaged refs, by pallet name.
/// * `Err(String)`         - If the user refs could not be listed.
pub fn detect(notes: &mut Vec<String>) -> Result<Vec<DamagedRef>, String> {
    let mut found: std::collections::BTreeMap<String, RefDamage> = std::collections::BTreeMap::new();

    for pallet in pallet_utils::list_pallets_in(PalletNamespace::User)? {
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, &pallet);

        match std::fs::read(&path) {
            Ok(bytes) => {
                if pallet_utils::parse_ref_bytes(&bytes).is_none() {
                    found.insert(pallet, RefDamage::Unreadable(bytes));
                }
            }
            Err(error) => push_note(notes, format!(
                "The ref of pallet \"{}\" could not be read to tell whether it is damaged: {}", pallet, error
            )),
        }
    }

    for (which, dir) in checkouts(notes).0 {
        match pallet_utils::read_position_in(&dir) {
            Ok(position) => {
                if position.base.is_some()
                    && !pallet_utils::get_pallet_ref_path(PalletNamespace::User, &position.pallet).exists()
                {
                    found.entry(position.pallet).or_insert(RefDamage::Missing);
                }
            }
            Err(error) => push_note(notes, format!(
                "Whether {} stands on a lost pallet ref could not be told: {}", which, error
            )),
        }
    }

    Ok(found.into_iter().map(|(pallet, damage)| DamagedRef { pallet, damage }).collect())
}

/// The hashes the durability taint records, and whether a torn taint leaves that set open.
struct Tainted {
    hashes: BTreeSet<String>,
    torn: bool,
}

impl Tainted {
    fn read(notes: &mut Vec<String>) -> Tainted {
        match taint_utils::read_taints(&forklift_root()) {
            Ok(state) => Tainted {
                hashes: state.recorded.iter().filter_map(|path| file_utils::hash_from_object_path(path)).collect(),
                torn: state.torn,
            },
            Err(error) => {
                push_note(notes, format!("The durability taint could not be read, so every object salvaged from a damaged ref counts as possibly lost: {}", error));
                Tainted { hashes: BTreeSet::new(), torn: true }
            }
        }
    }

    fn covers(&self, hash: &str) -> bool {
        self.torn || self.hashes.contains(hash)
    }
}

/// Gather what every checkout and the ref's own bytes say about `damaged`'s head.
fn gather(damaged: &DamagedRef, tainted: &Tainted, notes: &mut Vec<String>) -> Evidence {
    let pallet = damaged.pallet.as_str();
    let (checkouts, narrowed) = checkouts(notes);
    let mut evidence = Evidence { narrowed, ..Evidence::default() };

    for (which, dir) in checkouts {
        match pallet_utils::read_position_in(&dir) {
            Ok(position) if position.pallet == pallet => {
                evidence.stood_on = true;

                if let Some(base) = position.base {
                    evidence.candidates.push((base, Source::Base(which.clone())));
                }
            }
            Ok(_) => {}
            Err(error) => {
                evidence.narrowed = true;
                push_note(notes, format!("Whether {} stands on a lost pallet ref could not be told: {}", which, error));
            }
        }

        match journal_utils::read_in(&dir) {
            Ok(entries) => {
                if let Some(entry) = entries.iter().rev().find(|entry| entry.after.heads.contains_key(pallet)) {
                    match &entry.after.heads[pallet] {
                        Some(head) => evidence.candidates.push((head.clone(), Source::Journal(which.clone()))),
                        None => evidence.deleted_in.push(which.clone()),
                    }
                }
            }
            Err(error) => {
                evidence.narrowed = true;
                push_note(notes, format!("The journal of {} was not asked about the head of pallet \"{}\": {}", which, pallet, error));
            }
        }

        match operation_utils::read_in(&dir) {
            Ok(None) => {}
            Ok(Some(operation)) => {
                if operation.source.pallet == pallet {
                    if let Some(base) = &operation.source.base {
                        evidence.candidates.push((base.clone(), Source::Started(which.clone(), operation.kind)));
                    }
                }

                // A fast-forward's or lower's target stays with operation recovery, which checks
                // the preconditions it needs; a refused take is never landed here.
                if matches!(operation.kind, OperationKind::Merge | OperationKind::CherryPick)
                    && !operation.rolling_back
                    && operation.target.pallet == pallet
                {
                    if let Some(base) = &operation.target.base {
                        evidence.candidates.push((base.clone(), Source::Completing(which.clone(), operation.kind)));
                    }
                }
            }
            Err(error) => {
                evidence.narrowed = true;
                push_note(notes, format!("The interrupted operation of {} was not asked about the head of pallet \"{}\": {}", which, pallet, error));
            }
        }
    }

    if let RefDamage::Unreadable(bytes) = &damaged.damage {
        match salvage(bytes, tainted) {
            Ok(Salvage::Nothing) => {}
            Ok(Salvage::Found(head)) => evidence.candidates.push((head, Source::Salvage)),
            Ok(Salvage::Unknown(reason)) => evidence.salvage_unknown = Some(reason),
            Ok(Salvage::Ambiguous(parcels, dropped)) => {
                evidence.salvage_unknown = Some(format!("its bytes match more than one parcel: {}", parcels.join(", ")));
                evidence.salvage_ambiguous = parcels;
                evidence.salvage_dropped = dropped;
            }
            Err(error) => evidence.salvage_unknown = Some(format!("the store could not be searched for what its bytes name: {}", error)),
        }
    }

    evidence
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// What a damaged ref's `bytes` still name: the full hash when its 64 leading characters are hex
/// (a damaged newline), the one parcel matching at every position but the one non-hex character
/// among the first 64 (a flipped byte), else the one parcel a leading hex run of at least 8
/// characters is a prefix of (a truncation). A hash the taint records as possibly lost matches
/// as well as a stored one, so an object a crash took is an unknown answer rather than none.
fn salvage(bytes: &[u8], tainted: &Tainted) -> Result<Salvage, String> {
    let run = bytes.iter().take_while(|byte| is_hex(**byte)).count();
    let matching = |prefix: &str| -> Result<Vec<String>, String> {
        let mut matches = pallet_utils::object_hashes_with_prefix(prefix)?;
        matches.extend(tainted.hashes.iter().filter(|hash| hash.starts_with(prefix)).cloned());
        matches.sort();
        matches.dedup();
        Ok(matches)
    };

    if run >= HASH_LENGTH {
        return Ok(judge(matching(&String::from_utf8_lossy(&bytes[..HASH_LENGTH]))?, tainted));
    }

    let mut matches = Vec::new();

    if bytes.len() >= HASH_LENGTH && bytes[..HASH_LENGTH].iter().filter(|byte| !is_hex(**byte)).count() == 1 {
        for digit in b"0123456789abcdef" {
            let mut hash = bytes[..HASH_LENGTH].to_vec();
            hash[run] = *digit;

            matches.extend(matching(&String::from_utf8_lossy(&hash))?);
        }
    }

    if matches.is_empty() && run >= MIN_SALVAGE_PREFIX {
        matches = matching(&String::from_utf8_lossy(&bytes[..run]))?;
    }

    Ok(judge(matches, tainted))
}

/// The salvage among `matches`: the one readable parcel, unless another parcel also matches, or
/// a match is tainted or cannot be read as any kind of object, which leaves the answer unknown
/// (the bytes then still name a head, only not which). A
/// match that reads as a tree, blob, recipe or chunk is not a parcel and drops out.
fn judge(matches: Vec<String>, tainted: &Tainted) -> Salvage {
    let mut parcels = Vec::new();
    let mut unknown = Vec::new();

    for hash in matches {
        if tainted.covers(&hash) {
            unknown.push(hash);
        } else if object_utils::load_parcel(&hash).is_ok() {
            parcels.push(hash);
        } else if object_utils::load_tree(&hash).is_err() && object_utils::load_blob(&hash).is_err()
            && object_utils::load_recipe(&hash).is_err() && object_utils::load_chunk(&hash).is_err()
        {
            unknown.push(hash);
        }
    }

    if parcels.len() > 1 {
        return Salvage::Ambiguous(parcels, !unknown.is_empty());
    }

    if let Some(hash) = unknown.first() {
        return Salvage::Unknown(format!(
            "what its bytes name may be object {}, which is possibly lost or cannot be read", hash
        ));
    }

    match parcels.pop() {
        Some(hash) => Salvage::Found(hash),
        None => Salvage::Nothing,
    }
}

/// How a candidate head's closure checks out.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Closure {
    Verified,
    Incomplete,
    Unknown(String),
}

fn check_closure(pallet: &str, head: &str) -> Closure {
    let scope = match crate::util::scope_utils::read_fetch_scope() {
        Ok(scope) => scope,
        Err(error) => return Closure::Unknown(error),
    };

    match pallet_utils::verify_new_head(pallet, None, head, &scope) {
        Ok(_) => Closure::Verified,
        Err(RefUpdateError::Incomplete { .. }) => Closure::Incomplete,
        Err(error) => Closure::Unknown(error.to_string()),
    }
}

/// `ancestor` is `descendant` or one of its ancestors.
fn at_or_below(ancestor: &str, descendant: &str) -> Result<bool, String> {
    if ancestor == descendant {
        return Ok(true);
    }

    merge_utils::is_ancestor(ancestor, descendant)
}

/// What the choice settled on.
enum Choice {
    Rebuild(String, String),
    Defer,
    Refuse(String),
}

fn listed(candidates: &[(String, Source)]) -> Vec<Candidate> {
    candidates.iter().map(|(head, source)| Candidate { head: head.clone(), source: source.describe() }).collect()
}

/// Choose the head to rebuild `pallet` at from `evidence`.
fn choose(pallet: &str, evidence: &Evidence, pass: Pass) -> Choice {
    // A checkout standing on the pallet outweighs the delete: see the module doc.
    if !evidence.deleted_in.is_empty() && !evidence.stood_on {
        return Choice::Refuse(format!(
            "the latest journal entry of {} deleted the pallet, so a rebuild could undo that delete",
            evidence.deleted_in.join(" and ")
        ));
    }

    let mut heads: Vec<&str> = evidence.candidates.iter().map(|(head, _)| head.as_str()).collect();
    heads.sort();
    heads.dedup();

    let mut verified = Vec::new();
    let mut floors = Vec::new();
    let mut unknown: Option<String> = evidence.salvage_unknown.clone().map(|reason| format!("its salvage is unknown: {}", reason));
    let mut journal_unverified = false;

    for head in heads {
        let sources: Vec<&Source> = evidence.candidates.iter().filter(|(h, _)| h == head).map(|(_, source)| source).collect();
        let floor = sources.iter().any(|source| source.is_floor());

        match check_closure(pallet, head) {
            Closure::Verified => verified.push(head),
            Closure::Incomplete => {
                // A head only a journal names, which may have been rewound past or collected,
                // waits for the taint recovery once and then drops out.
                if !floor {
                    journal_unverified = true;
                }
            }
            Closure::Unknown(error) => {
                unknown.get_or_insert(format!("the history of {} could not be checked: {}", head, error));
            }
        }

        if floor {
            floors.push(head);
        }
    }

    let mut qualifier = None;

    'candidates: for &head in &verified {
        for &other in verified.iter().chain(floors.iter()) {
            match at_or_below(other, head) {
                Ok(true) => {}
                Ok(false) => continue 'candidates,
                Err(error) => {
                    unknown.get_or_insert(format!("whether {} descends from {} could not be told: {}", head, other, error));
                    continue 'candidates;
                }
            }
        }

        qualifier = Some(head);
        break;
    }

    if pass == Pass::BeforeTaint && (journal_unverified || unknown.is_some() || qualifier.is_none()) {
        return Choice::Defer;
    }

    if let Some(reason) = unknown {
        return Choice::Refuse(reason);
    }

    match qualifier {
        Some(head) => {
            let source = evidence.candidates.iter()
                .find(|(h, _)| h == head)
                .map(|(_, source)| source.describe())
                .unwrap_or_default();

            Choice::Rebuild(head.to_string(), source)
        }
        None if evidence.candidates.is_empty() => Choice::Refuse("nothing in this warehouse names its head".to_string()),
        None if verified.is_empty() => Choice::Refuse("no head the evidence names has its history complete in this warehouse".to_string()),
        None => Choice::Refuse("the heads the evidence names have diverged, or one a checkout stands on is not complete in this warehouse".to_string()),
    }
}

/// Run one repair pass over every damaged user ref.
///
/// # Returns
/// * `Ok(Repair)`  - What it rebuilt, and in [`Pass::AfterTaint`] what it refused.
/// * `Err(String)` - If the user refs could not be listed.
pub fn repair(pass: Pass) -> Result<Repair, String> {
    let mut report = Repair::default();
    let damaged = detect(&mut report.notes)?;

    if damaged.is_empty() {
        return Ok(report);
    }

    let tainted = Tainted::read(&mut report.notes);

    for damaged in damaged {
        let evidence = gather(&damaged, &tainted, &mut report.notes);

        match choose(&damaged.pallet, &evidence, pass) {
            Choice::Defer => {}
            Choice::Refuse(reason) => report.refused.push(Refused {
                pallet: damaged.pallet.clone(),
                reason,
                candidates: listed(&evidence.candidates),
            }),
            Choice::Rebuild(head, source) => match trusted(&damaged.pallet, &head)
                .map_err(RefUpdateError::Failed)
                .and_then(|preconditions| pallet_utils::repair_pallet_head(&damaged.pallet, &damaged.damage, &head, &preconditions))
            {
                Ok(()) => report.rebuilt.push(Rebuilt { pallet: damaged.pallet.clone(), head, source }),
                // Damaged tombstones refuse every ref write alike, and only the operator's repair
                // mends them, so the refusal is the answer, with its own next step. What this pass
                // already rebuilt stays reported.
                Err(RefUpdateError::Failed(error)) if crate::util::tombstone_utils::is_damage(&error) => {
                    report.stopped = Some(error);
                    break;
                }
                // The first pass leaves a failed write to the second, which reports it.
                Err(_) if pass == Pass::BeforeTaint => {}
                Err(error) => report.refused.push(Refused {
                    pallet: damaged.pallet.clone(),
                    reason: format!("the rebuild at {} was not written: {}", head, error),
                    candidates: listed(&evidence.candidates),
                }),
            },
        }
    }

    Ok(report)
}

/// The trust state a ref of `pallet` rebuilt or set at `head` is written under: in a warehouse
/// with an anchor, `head`'s history verifies against the anchor and office as they are now, as
/// an undone delete's does, since the written head is a local head no take verifies again.
fn trusted(pallet: &str, head: &str) -> Result<pallet_utils::RefPreconditions, String> {
    journal_utils::trust_preconditions_saying(
        &format!("The history of parcel {} does not pass this warehouse's trust check, so the ref of pallet \"{}\" is not set to it:", head, pallet),
        head,
        "Nothing was changed.",
    )
}

/// Find `pallet` among the damaged refs, refusing a ref that is not damaged.
fn damaged_ref(pallet: &str, notes: &mut Vec<String>) -> Result<DamagedRef, String> {
    pallet_utils::validate_pallet_name(pallet)?;

    detect(notes)?.into_iter().find(|damaged| damaged.pallet == pallet).ok_or_else(|| format!(
        "The ref of pallet \"{}\" is not damaged, so heal does not set or remove it. Nothing was changed.",
        pallet
    ))
}

/// `forklift heal --ref <pallet>=<parcel>`: set damaged `pallet`'s ref to `parcel` (a revision:
/// another pallet, a parcel hash or a unique prefix of one), through the same verified
/// compare-and-set as a rebuild.
///
/// A parcel of meta history (a meta pallet's head or one of its ancestors) is refused, since it
/// is not user history. In a served warehouse, whose clones may rely on the head the damage hides,
/// `parcel` must descend from every floor the evidence still records (see
/// [`refuse_below_a_floor`]): the hidden head itself cannot be checked.
pub fn set_damaged_ref(pallet: &str, parcel: &str, notes: &mut Vec<String>) -> Result<Rebuilt, String> {
    let (damaged, head, preconditions) = admit_set(pallet, parcel, notes)?;
    pallet_utils::repair_pallet_head(&damaged.pallet, &damaged.damage, &head, &preconditions).map_err(|error| error.to_string())?;

    Ok(Rebuilt { pallet: damaged.pallet, head, source: "the parcel given".to_string() })
}

/// Whether [`set_damaged_ref`] would refuse `pallet`'s ref at `parcel`, judged as it judges and
/// writing nothing: the parcel's full hash when it would not. Its `notes` explain a refusal; on
/// success the write says them again.
pub fn check_set_damaged_ref(pallet: &str, parcel: &str, notes: &mut Vec<String>) -> Result<String, String> {
    admit_set(pallet, parcel, notes).map(|(_, head, _)| head)
}

/// Every refusal of [`set_damaged_ref`], before its write: the damaged ref, the parcel's full
/// hash, and the preconditions the write holds it to.
fn admit_set(pallet: &str, parcel: &str, notes: &mut Vec<String>) -> Result<(DamagedRef, String, pallet_utils::RefPreconditions), String> {
    let damaged = damaged_ref(pallet, notes)?;

    let head = pallet_utils::resolve_revision(parcel)?;
    refuse_meta_history(pallet, parcel, &head, notes)?;

    // Damaged tombstones cannot tell whether the warehouse was served, so it is held to the floors
    // a served one is, with a note naming the repair.
    let served = crate::util::tombstone_utils::is_served().unwrap_or_else(|error| {
        push_note(notes, format!(
            "Whether this warehouse has been served could not be told, so it was taken as served: {}",
            crate::error::CoreError::from(error)
        ));
        true
    });

    if served {
        refuse_below_a_floor(&damaged, &head, notes)?;
    }

    let preconditions = trusted(&damaged.pallet, &head)?;
    Ok((damaged, head, preconditions))
}

/// Refuse `head`, resolved from `parcel`, when it is a meta pallet's head or one of its ancestors.
/// A meta pallet whose head or history cannot be read is not checked, with a note: refusing on it
/// would block every `--ref` over a meta pallet unrelated to the damaged ref.
fn refuse_meta_history(pallet: &str, parcel: &str, head: &str, notes: &mut Vec<String>) -> Result<(), String> {
    let mut unchecked = |meta: &str, error: String| push_note(notes, format!(
        "Whether {} is history of meta pallet \"@{}\" could not be told, so that was not checked: {}", head, meta, error
    ));

    for meta in pallet_utils::list_meta_pallets()? {
        let meta_head = match pallet_utils::get_meta_pallet_head(&meta) {
            Ok(Some(meta_head)) => meta_head,
            Ok(None) => continue,
            Err(error) => {
                unchecked(&meta, error);
                continue;
            }
        };

        match at_or_below(head, &meta_head) {
            Ok(false) => {}
            Ok(true) => return Err(format!(
                "\"{}\" is meta history (of pallet \"@{}\"), not user history, so the ref of pallet \"{}\" is not set to it. Nothing was changed.",
                parcel, meta, pallet
            )),
            Err(error) => unchecked(&meta, error),
        }
    }

    Ok(())
}

/// Refuse `head` for served `damaged` unless it equals or descends from every floor the evidence
/// still records (each checkout's base, an interrupted operation's parcels, the one parcel the
/// ref's own bytes name) and, when those bytes match several parcels, from at least one of them.
/// This is judged before the write's hold, and only over what can be read: a checkout whose state
/// cannot be read, a salvage match that is lost, or an ancestry walk that fails adds a note
/// instead, since refusing on them would leave the ref no way out. With no floor at all it passes:
/// what clones fetched is then the operator's to know.
fn refuse_below_a_floor(damaged: &DamagedRef, head: &str, notes: &mut Vec<String>) -> Result<(), String> {
    let evidence = gather(damaged, &Tainted::read(notes), notes);
    let mut partial = evidence.narrowed || evidence.salvage_dropped
        || (evidence.salvage_unknown.is_some() && evidence.salvage_ambiguous.is_empty());

    let below = |what: String| format!(
        "This warehouse is served, and {} does not descend from {}, so the ref of pallet \"{}\" is not set to it: its clones may rely on that head. Nothing was changed.",
        head, what, damaged.pallet
    );

    for (floor, source) in evidence.candidates.iter().filter(|(_, source)| source.is_floor()) {
        match at_or_below(floor, head) {
            Ok(true) => {}
            Ok(false) => return Err(below(format!("{} ({})", floor, source.describe()))),
            Err(_) => partial = true,
        }
    }

    if !evidence.salvage_ambiguous.is_empty() {
        let mut below_one = None;
        let mut unanswered = false;

        for parcel in &evidence.salvage_ambiguous {
            match at_or_below(parcel, head) {
                Ok(true) => {
                    below_one = Some(parcel);
                    break;
                }
                Ok(false) => {}
                Err(_) => unanswered = true,
            }
        }

        partial |= unanswered;

        // Which match the bytes name cannot be told, so the one satisfied is said.
        if let Some(parcel) = below_one {
            push_note(notes, format!(
                "This warehouse is served, and the ref of pallet \"{}\" may have named any of {}: {} was checked only against {}.",
                damaged.pallet, evidence.salvage_ambiguous.join(", "), head, parcel
            ));
        }

        // A lost match may be the one the bytes name, so only a "no" from every match refuses.
        if below_one.is_none() && !unanswered && !evidence.salvage_dropped {
            return Err(below(format!(
                "any of the parcels the ref's own bytes may name ({})", evidence.salvage_ambiguous.join(", ")
            )));
        }
    }

    if partial {
        push_note(notes, format!(
            "This warehouse is served, and not everything that records the head of pallet \"{}\" could be read, so {} was checked only against what could.",
            damaged.pallet, head
        ));
    }

    Ok(())
}

/// A ref `--remove-ref` removed, and where its bytes were kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
pub struct Removed {
    /// The pallet.
    pub pallet: String,

    /// The file its unreadable bytes were moved to.
    pub kept: String,
}

/// The folder unreadable refs that `--remove-ref` removed are kept in.
const FOLDER_NAME_REMOVED_REFS: &str = "removed-refs";

/// `forklift heal --remove-ref <pallet>`: remove damaged `pallet`'s unreadable ref, keeping its
/// bytes in a file of their own. Allowed only when no checkout stands on the pallet, nothing names
/// a head for it (its own bytes included), and every checkout's state could be read.
pub fn remove_damaged_ref(pallet: &str, notes: &mut Vec<String>) -> Result<Removed, String> {
    let damaged = admit_remove(pallet, notes)?;
    let RefDamage::Unreadable(bytes) = &damaged.damage else {
        unreachable!("admit_remove admits only an unreadable ref");
    };

    let folder = forklift_root().join(FOLDER_NAME_REMOVED_REFS);
    let stem = pallet.replace('/', "%2F");
    let kept = (0..).map(|n| folder.join(match n { 0 => stem.clone(), n => format!("{}.{}", stem, n) }))
        .find(|path| !path.exists())
        .expect("an unbounded range always yields a free name");

    pallet_utils::remove_damaged_ref(pallet, bytes, &kept).map_err(|error| error.to_string())?;

    Ok(Removed { pallet: pallet.to_string(), kept: kept.to_string_lossy().into_owned() })
}

/// Whether [`remove_damaged_ref`] would refuse to remove `pallet`'s ref, judged as it judges and
/// writing nothing. Its `notes` explain a refusal; on success the removal says them again.
pub fn check_remove_damaged_ref(pallet: &str, notes: &mut Vec<String>) -> Result<(), String> {
    admit_remove(pallet, notes).map(|_| ())
}

/// Every refusal of [`remove_damaged_ref`], before its write: the damaged ref, always unreadable.
fn admit_remove(pallet: &str, notes: &mut Vec<String>) -> Result<DamagedRef, String> {
    let damaged = damaged_ref(pallet, notes)?;

    let RefDamage::Unreadable(_) = &damaged.damage else {
        return Err(format!(
            "Pallet \"{}\" has no ref to remove: a checkout stands on it, so heal rebuilds it instead. Nothing was changed.",
            pallet
        ));
    };

    let tainted = Tainted::read(notes);
    let evidence = gather(&damaged, &tainted, notes);

    if evidence.stood_on {
        return Err(format!("A checkout stands on pallet \"{}\", so its ref is not removed. Nothing was changed.", pallet));
    }

    if evidence.narrowed {
        return Err(format!(
            "The state of a checkout could not be read, so whether anything names a head for pallet \"{}\" cannot be told. Nothing was changed.",
            pallet
        ));
    }

    if !evidence.candidates.is_empty() || evidence.salvage_unknown.is_some() || !evidence.deleted_in.is_empty() {
        let named: Vec<String> = listed(&evidence.candidates).into_iter()
            .map(|candidate| format!("{} ({})", candidate.head, candidate.source))
            .collect();

        return Err(format!(
            "Something still names a head for pallet \"{}\"{}, so its ref is not removed: set it with \"forklift heal --ref {}=<parcel>\". Nothing was changed.",
            pallet,
            if named.is_empty() { String::new() } else { format!(": {}", named.join(", ")) },
            pallet,
        ));
    }

    Ok(damaged)
}

/// A meta pallet's ref and its copy, as heal read them before any hold.
struct MetaState {
    name: String,

    /// The ref file's bytes; `None` when nothing stands at the ref's path.
    reference: Option<Vec<u8>>,

    copy: pallet_utils::MetaCopyState,
}

impl MetaState {
    fn wire(&self) -> String {
        meta_wire(&self.name)
    }

    /// How the ref is damaged: lost beside a copy, or holding bytes that are not a head. `None`
    /// for an intact ref, and for a pallet with neither a ref nor a copy, which is unborn.
    fn damage(&self) -> Option<RefDamage> {
        match &self.reference {
            Some(bytes) if pallet_utils::parse_ref_bytes(bytes).is_some() => None,
            Some(bytes) => Some(RefDamage::Unreadable(bytes.clone())),
            None if self.copy == pallet_utils::MetaCopyState::Absent => None,
            None => Some(RefDamage::Missing),
        }
    }

    fn copy_head(&self) -> Option<String> {
        match &self.copy {
            pallet_utils::MetaCopyState::Bytes(bytes) => pallet_utils::parse_ref_bytes(bytes),
            _ => None,
        }
    }
}

fn meta_wire(name: &str) -> String {
    pallet_utils::PalletRef { namespace: PalletNamespace::Meta, name: name.to_string() }.to_wire()
}

/// Every meta pallet this forklift knows that has something at its ref or its copy, read without
/// the ref lock. The names are looked up, never the folders listed, so a stray file in either is
/// neither a pallet nor a reason to stop. One whose ref or copy cannot be read is left out, with a
/// note, and so is one with no ref and something other than a file at its copy, which names no
/// head and so leaves the pallet unborn.
fn meta_states(notes: &mut Vec<String>) -> Result<Vec<MetaState>, String> {
    let present = |path: std::path::PathBuf| !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    let names = crate::util::meta_tree::META_PALLET_NAMES.iter()
        .filter(|name| present(pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, name)) || present(pallet_utils::get_meta_copy_path(name)))
        .map(|name| name.to_string());

    let mut states = Vec::new();

    for name in names {
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, &name);

        let reference = match pallet_utils::read_file_state(&path) {
            Ok(pallet_utils::MetaCopyState::Absent) => None,
            Ok(pallet_utils::MetaCopyState::Bytes(bytes)) => Some(bytes),
            Ok(pallet_utils::MetaCopyState::Other) => {
                push_note(notes, format!(
                    "Something that is not a file stands at the ref of pallet \"{}\" (\"{}\"), so heal did not judge it.",
                    meta_wire(&name), path.to_string_lossy()
                ));
                continue;
            }
            Err(error) => {
                push_note(notes, format!("The ref of pallet \"{}\" could not be read to tell whether it is damaged: {}", meta_wire(&name), error));
                continue;
            }
        };

        match pallet_utils::read_meta_copy_state(&name) {
            Ok(pallet_utils::MetaCopyState::Other) if reference.is_none() => push_note(notes, format!(
                "Something that is not a file stands at the copy of pallet \"{}\"'s ref (\"{}\"), which has no ref: it names no head, so heal did not judge that ref.",
                meta_wire(&name), pallet_utils::get_meta_copy_path(&name).to_string_lossy()
            )),
            Ok(copy) => states.push(MetaState { name, reference, copy }),
            Err(error) => push_note(notes, format!(
                "The copy of pallet \"{}\"'s ref could not be read, so heal did not judge that ref: {}", meta_wire(&name), error
            )),
        }
    }

    Ok(states)
}

/// Whether `head` is `ancestor` or descends from it along first parents.
fn first_parent_descends(head: &str, ancestor: &str) -> Result<bool, String> {
    let mut at = head.to_string();
    let mut seen = BTreeSet::new();

    loop {
        if at == ancestor {
            return Ok(true);
        }

        if !seen.insert(at.clone()) {
            return Ok(false);
        }

        match object_utils::load_parcel(&at)?.parents.first() {
            Some(parent) => at = parent.clone(),
            None => return Ok(false),
        }
    }
}

const FROM_COPY: &str = "the ref's copy";

/// The head a damaged meta ref is rebuilt at, and where it was found, or why none is.
///
/// A copy that differs is taken as ahead of the ref because a forward write moves the copy first.
/// Undo's put-back moves a ref backward and writes the copy second, so one interrupted between
/// the two leaves the copy at the head it was undoing, and this would rebuild there. Only a
/// journaled command's refs are put back, and none of `journal_op`'s commands (stack, consolidate,
/// pick, shift) writes a meta ref: `git grep -l -E 'update_meta_pallet_head|PalletNamespace::Meta'`
/// over their command and util modules finds none.
fn choose_meta(state: &MetaState, tainted: &Tainted) -> Result<(String, &'static str), (String, Vec<Candidate>)> {
    let copy = state.copy_head();
    let mut candidates: Vec<Candidate> = copy.iter()
        .map(|head| Candidate { head: head.clone(), source: FROM_COPY.to_string() })
        .collect();

    let salvaged = match &state.reference {
        Some(bytes) => salvage(bytes, tainted).map_err(|error| (
            format!("what its own bytes name could not be looked up: {}", error.trim_end_matches('.')), candidates.clone()
        ))?,
        None => Salvage::Nothing,
    };

    match salvaged {
        Salvage::Nothing => match copy {
            Some(head) => Ok((head, FROM_COPY)),
            None => Err(("neither its copy nor its own bytes name a head".to_string(), candidates)),
        },
        Salvage::Found(salvaged) => {
            candidates.push(Candidate { head: salvaged.clone(), source: Source::Salvage.describe() });

            match copy {
                None => Ok((salvaged, "what the ref's own bytes still name")),
                Some(copy) if copy == salvaged => Ok((copy, FROM_COPY)),
                // A copy a forward write left ahead of its ref is in this shape; one an interrupted
                // re-genesis or a hand edit left need not be, and nothing proves it the newer.
                Some(copy) => match first_parent_descends(&copy, &salvaged) {
                    Ok(true) => Ok((copy, FROM_COPY)),
                    Ok(false) => Err((format!(
                        "its copy names {}, which does not descend along first parents from {}, what its own bytes name, so nothing proves either the newer",
                        copy, salvaged
                    ), candidates)),
                    Err(error) => Err((format!(
                        "whether its copy's head {} descends from {}, what its own bytes name, could not be told: {}",
                        copy, salvaged, error.trim_end_matches('.')
                    ), candidates)),
                },
            }
        }
        Salvage::Ambiguous(parcels, _) => {
            candidates.extend(parcels.into_iter().map(|head| Candidate { head, source: Source::Salvage.describe() }));
            Err(("its own bytes name one of several parcels, not which".to_string(), candidates))
        }
        Salvage::Unknown(why) => Err((why, candidates)),
    }
}

/// Verify `head` as the whole history of meta pallet `name`, against the trust anchor and the
/// office as they are now, and return the preconditions that pin both for the write.
fn verify_meta_head(name: &str, head: &str) -> Result<pallet_utils::RefPreconditions, String> {
    let Some(rules) = crate::util::meta_tree::meta_rules(name) else {
        return Err("it is not a meta pallet heal has rules to verify against".to_string());
    };

    let anchor = crate::util::office_utils::read_trust_anchor_with_bytes()?;
    let office_head = pallet_utils::get_meta_pallet_head(crate::util::office_utils::OFFICE_PALLET_NAME)?;

    crate::util::meta_tree::verify_meta_update(
        name, rules, crate::util::meta_tree::Admission::Audit, anchor.as_ref().map(|(anchor, _)| anchor),
        office_head.as_deref(), None, head,
    ).map_err(|why| format!("parcel {} does not pass verification: {}", head, why.trim_end_matches('.')))?;

    Ok(pallet_utils::RefPreconditions {
        office: pallet_utils::OfficePrecondition::At(office_head),
        anchor: match anchor {
            Some((_, bytes)) => pallet_utils::AnchorPrecondition::Bytes(bytes),
            None => pallet_utils::AnchorPrecondition::Absent,
        },
        pallets: pallet_utils::PalletsPrecondition::NotConsumed,
    })
}

/// Set an intact meta ref's copy to the ref's bytes, when the copy is lost or differs.
fn mend_copy(state: &MetaState, head: &str, pass: Pass, notes: &mut Vec<String>) {
    let reference = state.reference.as_deref().unwrap_or_default();

    if let pallet_utils::MetaCopyState::Other = state.copy {
        if pass == Pass::AfterTaint {
            push_note(notes, format!(
                "Something that is not a file stands at the copy of pallet \"{}\"'s ref (\"{}\"), so the copy was not set to the ref's head {}; move it aside and run \"forklift heal\" again.",
                state.wire(), pallet_utils::get_meta_copy_path(&state.name).to_string_lossy(), head
            ));
        }
        return;
    }

    match pallet_utils::mend_meta_copy(&state.name, reference, &state.copy) {
        Ok(pallet_utils::CopyMend::Mended) => push_note(notes, match state.copy_head() {
            Some(other) if other == head => format!(
                "The copy of pallet \"{}\"'s ref named the ref's head {}, in other bytes; it was set to the ref's bytes.",
                state.wire(), head
            ),
            Some(other) => format!(
                "The copy of pallet \"{}\"'s ref named {}, not the ref's head {}, as an interrupted write or a hand edit leaves it; it was set to the ref's head.",
                state.wire(), other, head
            ),
            None => format!("The copy of pallet \"{}\"'s ref was lost or damaged; it was set to the ref's head {}.", state.wire(), head),
        }),
        // A taint still standing after the recovery is reported by the recovery.
        Ok(pallet_utils::CopyMend::AfterTaint) => {}
        Err(_) if pass == Pass::BeforeTaint => {}
        Err(error) => push_note(notes, format!(
            "The copy of pallet \"{}\"'s ref was not set to the ref's head {}: {}", state.wire(), head, error
        )),
    }
}

/// What a meta repair pass did: the meta refs' [`Repair`], and the trust file repair run between
/// the office's rebuild and the other meta refs'.
#[derive(Debug, Default)]
pub struct MetaRepair {
    pub repair: Repair,
    pub trust: crate::util::office_utils::TrustRepair,
}

/// A meta repair pass that stopped at the trust file repair: its refusal, and the office's
/// rebuild when that landed before it.
#[derive(Debug)]
pub struct TrustStopped {
    pub error: String,
    pub rebuilt: Vec<Rebuilt>,
}

fn is_office(state: &MetaState) -> bool {
    state.name == crate::util::office_utils::OFFICE_PALLET_NAME
}

fn is_redactions(state: &MetaState) -> bool {
    state.name == crate::util::redaction_utils::REDACTIONS_PALLET_NAME
}

/// Run one repair pass over every meta ref, in three steps:
/// 1. A lost or damaged office ref is rebuilt from its copy or its own bytes, at a head whose
///    chain verifies against the anchor of the trust file or, failing that, of its copy, while
///    `@redactions` reads. The write is pinned to the bytes of the file that anchor came from,
///    not to the office, whose ref is what is being rebuilt.
/// 2. The trust file and its copy are repaired ([`repair_trust`]), reading the office as step 1
///    left it, so a trust file lost beside a lost office is restored once the office stands.
/// 3. Every meta ref that reads has a lost or differing copy set to it, and a lost or damaged
///    ref other than the office's and `@redactions'` is rebuilt from its copy or its own bytes,
///    verified against the anchor and the office as step 2 left them, while both of those read.
///    A lost or damaged `@redactions` ref is refused, and so is every other damaged meta ref
///    beside it or beside an office step 1 did not rebuild, since each is verified against the
///    office and checked against the redactions.
///
/// [`Pass::BeforeTaint`] refuses nothing and drops a write that failed, leaving both to
/// [`Pass::AfterTaint`], which judges again.
///
/// [`repair_trust`]: crate::util::office_utils::repair_trust
///
/// # Returns
/// * `Ok(MetaRepair)`      - What it rebuilt, and in [`Pass::AfterTaint`] what it refused; and
///                           what the trust file repair wrote and noted.
/// * `Err(TrustStopped)`   - The trust file repair's refusal, with the office rebuild that
///                           landed before it.
pub fn repair_meta(pass: Pass) -> Result<MetaRepair, TrustStopped> {
    let mut report = Repair::default();
    let mut tainted = None;

    // Never fails today (what cannot be read is noted), so the trust repair below always runs.
    let states = meta_states(&mut report.notes).unwrap_or_default();
    let redactions_damaged = states.iter().any(|state| is_redactions(state) && state.damage().is_some());
    let office = states.iter().find(|state| is_office(state) && state.damage().is_some());
    let office_judged = office.is_some() && !redactions_damaged;

    if let (Some(state), false) = (office, redactions_damaged) {
        let tainted = tainted.get_or_insert_with(|| Tainted::read(&mut report.notes));
        rebuild_office(state, tainted, pass, &mut report);
    }

    let trust = crate::util::office_utils::repair_trust()
        .map_err(|error| TrustStopped { error, rebuilt: report.rebuilt.clone() })?;

    if report.stopped.is_none() {
        repair_other_meta(pass, office_judged, &mut tainted, &mut report);
    }

    Ok(MetaRepair { repair: report, trust })
}

/// Step 1 of [`repair_meta`]: judge the damaged office ref `state` and write it.
fn rebuild_office(state: &MetaState, tainted: &Tainted, pass: Pass, report: &mut Repair) {
    let refuse = |report: &mut Repair, reason: String, candidates: Vec<Candidate>| if pass == Pass::AfterTaint {
        report.refused.push(Refused { pallet: state.wire(), reason, candidates });
    };

    let (head, source) = match choose_meta(state, tainted) {
        Ok(chosen) => chosen,
        Err((reason, candidates)) => return refuse(report, reason, candidates),
    };
    let candidates = vec![Candidate { head: head.clone(), source: source.to_string() }];

    // The trust file's anchor first, so a copy is used only when the trust file is lost or does
    // not verify the head. The head is never chosen by the anchor: a head neither verifies is
    // refused.
    let anchors = crate::util::office_utils::trust_file_anchors(&mut report.notes);
    let mut failures = Vec::new();
    let mut verified = None;

    for (kind, anchor, bytes) in anchors {
        let file = match kind {
            crate::util::office_utils::TrustFileKind::Trust => "the trust file",
            crate::util::office_utils::TrustFileKind::Copy => "the trust file's copy",
        };

        match crate::util::audit_utils::verify_office_chain(&anchor, &head) {
            Ok(_) => {
                verified = Some(match kind {
                    crate::util::office_utils::TrustFileKind::Trust => pallet_utils::AnchorPrecondition::Bytes(bytes),
                    crate::util::office_utils::TrustFileKind::Copy => pallet_utils::AnchorPrecondition::CopyBytes(bytes),
                });
                break;
            }
            Err(why) => failures.push(format!("against the anchor of {}: {}", file, why.to_string().trim_end_matches('.'))),
        }
    }

    let Some(anchor) = verified else {
        return refuse(report, match failures.is_empty() {
            true => format!("parcel {} cannot be verified: neither the trust file nor its copy holds an anchor heal can read", head),
            false => format!("parcel {} does not verify as the office's history {}", head, failures.join("; nor ")),
        }, candidates);
    };

    let preconditions = pallet_utils::RefPreconditions {
        office: pallet_utils::OfficePrecondition::NotConsumed,
        anchor,
        pallets: pallet_utils::PalletsPrecondition::NotConsumed,
    };

    write_meta_rebuild(state, head, source, &preconditions, pass, report);
}

/// Write the rebuild of damaged meta ref `state` at `head`, found at `source`, and report it.
///
/// # Returns
/// * `true` - If the pass stops here (`report.stopped` says why).
fn write_meta_rebuild(state: &MetaState, head: String, source: &str, preconditions: &pallet_utils::RefPreconditions, pass: Pass, report: &mut Repair) -> bool {
    let damage = state.damage().expect("only a damaged ref is rebuilt");
    let candidates = vec![Candidate { head: head.clone(), source: source.to_string() }];
    let written = pallet_utils::repair_meta_pallet_head(&state.name, &damage, &state.copy, &head, preconditions);

    // A write can land and still report a failure (under a standing taint, its rename goes
    // through and the taint is reported), so the ref read back decides.
    let landed = pallet_utils::get_pallet_head_in(PalletNamespace::Meta, &state.name).ok().flatten().as_deref() == Some(head.as_str());

    let reason = match written {
        _ if landed => {
            report.rebuilt.push(Rebuilt { pallet: state.wire(), head, source: source.to_string() });
            return false;
        }
        // As in the user pass: the refusal is the answer, and what this pass rebuilt stays
        // reported.
        Err(RefUpdateError::Failed(error)) if crate::util::tombstone_utils::is_damage(&error) => {
            report.stopped = Some(error);
            return true;
        }
        Err(_) if pass == Pass::BeforeTaint => return false,
        Err(error) => format!("the rebuild at {} was not written: {}", head, error),
        Ok(()) => format!("the rebuild at {} did not read back", head),
    };

    if pass == Pass::AfterTaint {
        report.refused.push(Refused { pallet: state.wire(), reason, candidates });
    }

    false
}

/// Step 3 of [`repair_meta`], over the meta refs as the steps before it left them. The office is
/// skipped when step 1 judged it (`office_judged`), which reported it.
fn repair_other_meta(pass: Pass, office_judged: bool, tainted: &mut Option<Tainted>, report: &mut Repair) {
    let states = meta_states(&mut report.notes).unwrap_or_default();

    let blocking: Vec<String> = states.iter()
        .filter(|state| state.damage().is_some() && (is_office(state) || is_redactions(state)))
        .map(MetaState::wire)
        .collect();

    if states.iter().any(|state| state.damage().is_some()) && tainted.is_none() {
        *tainted = Some(Tainted::read(&mut report.notes));
    }

    for state in &states {
        let Some(_) = state.damage() else {
            if let Some(head) = state.reference.as_deref().and_then(pallet_utils::parse_ref_bytes) {
                if state.copy != pallet_utils::MetaCopyState::Bytes(state.reference.clone().unwrap_or_default()) {
                    mend_copy(state, &head, pass, &mut report.notes);
                }
            }
            continue;
        };

        if office_judged && is_office(state) {
            continue;
        }

        let mut refuse = |reason: String, candidates: Vec<Candidate>| if pass == Pass::AfterTaint {
            report.refused.push(Refused { pallet: state.wire(), reason, candidates });
        };

        let choice = choose_meta(state, tainted.as_ref().expect("read when a ref is damaged"));

        if is_redactions(state) || (is_office(state) && blocking.contains(&state.wire())) {
            let candidates = match choice {
                Ok((head, source)) => vec![Candidate { head, source: source.to_string() }],
                Err((_, candidates)) => candidates,
            };
            refuse("heal does not rebuild this ref; write the head it is to hold into the ref file by hand".to_string(), candidates);
            continue;
        }

        let (head, source) = match choice {
            Ok(chosen) => chosen,
            Err((reason, candidates)) => {
                refuse(reason, candidates);
                continue;
            }
        };
        let candidates = vec![Candidate { head: head.clone(), source: source.to_string() }];

        if !blocking.is_empty() {
            refuse(format!(
                "it is verified against the office and checked against the redactions, and the ref of {} is damaged too: set that first",
                blocking.iter().map(|wire| format!("pallet \"{}\"", wire)).collect::<Vec<_>>().join(" and ")
            ), candidates);
            continue;
        }

        let preconditions = match verify_meta_head(&state.name, &head) {
            Ok(preconditions) => preconditions,
            Err(reason) => {
                refuse(reason, candidates);
                continue;
            }
        };

        if write_meta_rebuild(state, head, source, &preconditions, pass, report) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::pallet_utils::tests::{scratch, stub_parcel};

    // Mutation: judge two matching parcels as Nothing → the ref's bytes stop being a floor and
    // `--remove-ref` may drop a ref they name → red.
    #[test]
    fn bytes_matching_more_than_one_parcel_are_unknown_not_nothing() {
        let (_root, _scope) = scratch("ref-heal-judge");
        let (one, two) = (stub_parcel("1"), stub_parcel("2"));
        let clean = Tainted { hashes: BTreeSet::new(), torn: false };

        assert_eq!(judge(vec![one.clone()], &clean), Salvage::Found(one.clone()));
        assert_eq!(judge(vec![one.clone(), two.clone()], &clean), Salvage::Ambiguous(vec![one.clone(), two.clone()], false));

        let tainted = Tainted { hashes: BTreeSet::from(["f".repeat(HASH_LENGTH)]), torn: false };
        assert_eq!(judge(vec![one.clone(), two.clone(), "f".repeat(HASH_LENGTH)], &tainted), Salvage::Ambiguous(vec![one, two], true));
    }
}
