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
use std::path::{Path, PathBuf};

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

    /// For a meta ref, whose refusal it is and the files it was judged by; `None` for a user ref.
    #[serde(skip)]
    #[cfg_attr(feature = "docgen", schemars(skip))]
    pub meta: Option<MetaVerdict>,
}

/// Whose refusal a meta ref's is, which decides the step that lets a later heal judge it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The ref's own: nothing heal can verify names a head for it, so its removal is the way out.
    /// Only a named check gives one, as the [`Verdict`] it carries says; every other refusal is
    /// one of the kinds below, which the removal refuses.
    Own(Verdict),

    /// It waits on the refusal of this meta ref (`@office` or `@redactions`), which it is
    /// verified against or checked against.
    Cascade(String),

    /// The office is unborn while the trust file holds an anchor, so nothing a meta ref holds can
    /// be verified until the office stands.
    OfficeUnborn,

    /// What heal needs to judge it reads whole but is in a format or version this build does not
    /// know: a newer Forklift may have written it, so it is no verdict on the ref. The way out is
    /// a build that reads it ([`newer_step`]).
    Newer,

    /// The trust state it is verified against reads whole but refuses: the office stands while
    /// no trust file holds its anchor, or the office chain does not verify against the anchor. No
    /// verdict on the ref, and nothing a rerun changes until that is settled; the way out is the
    /// step it carries, a command or sentences as [`meta_step`] returns them.
    Trust(String),

    /// What heal needs to judge it could not be read, or its rebuild could not be written: no
    /// verdict on the ref, and not one a rerun alone changes. The way out is the step the read's
    /// own refusal named, when it named one other than heal itself (a command or sentences as
    /// [`meta_step`] returns them), else to settle what the refusal names ([`unread_step`]).
    /// Producers pass `None`; [`refuse_in`] lifts the step out of the reason.
    Unread(Option<String>),

    /// Heal chose its head but could not write the rebuild, or could not read the ref back after
    /// writing it: no verdict on the ref. A rerun mends a passing fault and meets a lasting one
    /// again, so the way out is the step the write's own refusal named, when it named one other
    /// than heal itself, else to settle why it failed ([`unwritten_step`]). Producers pass
    /// `None`; [`refuse_in`] lifts the step out of the reason.
    Unwritten(Option<String>),

    /// Its files moved while heal ran: a later heal judges it again.
    Again,
}

/// A verdict on a meta ref's own history: the checks whose refusal admits its removal, and no
/// others. A refusal no check below gives (a read that failed, a string some step lifted, damage
/// to a file beside the history, such as a signature's) is never one, so it never reaches the
/// removal: what is not named here is refused by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Neither its copy nor its own bytes name a head.
    NoHead,

    /// Its own bytes name one of several parcels, not which.
    AmbiguousHead,

    /// What its own bytes name is a match under a taint, or one the store decided is absent,
    /// damaged or another kind of object.
    UnknownHead,

    /// Its copy and what its own bytes name are both parcels, and neither descends from the
    /// other along first parents.
    Diverged,

    /// A parcel or object of its history the store decided about, as the [`Decided`] says.
    ///
    /// [`Decided`]: crate::util::file_utils::Decided
    Object(crate::util::file_utils::Decided),

    /// A tree of its history is no meta tree: it names no format (as an older forklift's shape
    /// does), or its spine does not read as one.
    NotASpine,

    /// A tree of its history is a meta tree of a kind this build knows, and not this pallet's.
    OtherSpine,

    /// A tree or cell of its history reads whole but is not in canonical form.
    Malformed,

    /// A parcel of its history breaks a rule of the meta format or of the pallet.
    RuleBroken,

    /// A parcel of its history is not signed as its trust state requires: a signature that
    /// decodes and does not verify, by a key the office does not track, or by a revoked key outside
    /// its boundary; none after trust; one with no office to verify it; or, unsigned, no one
    /// operator to attribute it to.
    SignatureRefused,

    /// For the office: the anchor of each trust file reads and does not verify its history.
    AnchorRefused,

    /// It is no meta pallet heal has rules to verify against.
    NoRules,
}

impl Verdict {
    /// The verdict, in words, to follow "heal refused it as its own:".
    pub fn describe(self) -> &'static str {
        use crate::util::file_utils::Decided;

        match self {
            Verdict::NoHead => "nothing names a head for it",
            Verdict::AmbiguousHead => "its own bytes name one of several parcels",
            Verdict::UnknownHead => "what its own bytes name is possibly lost",
            Verdict::Diverged => "its copy and its own bytes name heads that diverge",
            Verdict::Object(Decided::Absent) => "the store holds no object of its history it needs",
            Verdict::Object(Decided::Damaged) => "the store holds an object of its history damaged",
            Verdict::Object(Decided::OtherKind) => "an object of its history is of another kind",
            Verdict::NotASpine => "a tree of its history is no meta tree",
            Verdict::OtherSpine => "a tree of its history is a meta tree of another kind",
            Verdict::Malformed => "a tree or cell of its history is not in canonical form",
            Verdict::RuleBroken => "a parcel of its history breaks a rule",
            Verdict::SignatureRefused => "a parcel of its history is not signed as trust requires",
            Verdict::AnchorRefused => "no trust file's anchor verifies its history",
            Verdict::NoRules => "heal has no rules to verify it against",
        }
    }
}

/// A meta ref's refusal as heal judged it: whose it is, and the ref and copy it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaVerdict {
    pub owner: Owner,
    damage: RefDamage,
    copy: pallet_utils::MetaCopyState,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repair {
    /// The refs it rebuilt.
    pub rebuilt: Vec<Rebuilt>,

    /// The refs it refused to rebuild (only [`Pass::AfterTaint`] refuses; the first pass defers).
    pub refused: Vec<Refused>,

    /// The meta refs [`Pass::BeforeTaint`] judged and left for [`Pass::AfterTaint`], each with
    /// why and every head it found: a run that stops before that pass names them.
    pub deferred: Vec<Refused>,

    /// The lost or damaged meta refs, as `@<name>`, that a meta pass which stopped never judged:
    /// neither rebuilt, refused nor deferred.
    pub unjudged: Vec<String>,

    /// The deferred meta refs, as `@<name>`, whose own bytes name only objects that a taint whose
    /// record is torn, or could not be read, counts as possibly lost; and which of the two it was.
    pub held: Option<TaintHold>,

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

    /// A match whose kind cannot be read, where the store's read said nothing of the object (it
    /// could not be consulted, or left the answer undecided), or a pack that may hold a match the
    /// store cannot list. What heal could not read is no verdict on the ref.
    Untold(String),

    /// A match the store gave whole that this build cannot parse: a newer Forklift may have
    /// written it, so it is no verdict on the ref.
    Newer(String),
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

/// The note for the ref of `pallet` (as it is written on the wire) at `path`, which heal leaves
/// unjudged since something that is not a file stands there.
fn not_a_file_note(pallet: &str, path: &Path) -> String {
    format!(
        "Something that is not a file stands at the ref of pallet \"{}\" (\"{}\"), so heal did not judge it. \
        Move it aside, then run heal again.",
        pallet, path.to_string_lossy()
    )
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

        // Judged as it stands, never through a link, as every read of the ref judges it.
        match pallet_utils::read_file_state(&path) {
            Ok(pallet_utils::MetaCopyState::Bytes(bytes)) => {
                if pallet_utils::parse_ref_bytes(&bytes).is_none() {
                    found.insert(pallet, RefDamage::Unreadable(bytes));
                }
            }
            Ok(pallet_utils::MetaCopyState::Absent) => {}
            Ok(pallet_utils::MetaCopyState::Other) => push_note(notes, not_a_file_note(&pallet, &path)),
            Err(error) => push_note(notes, format!(
                "The ref of pallet \"{}\" could not be read to tell whether it is damaged: {}", pallet, error
            )),
        }
    }

    for (which, dir) in checkouts(notes).0 {
        match pallet_utils::read_position_in(&dir) {
            // Judged as it stands, as above: whatever stands at the ref's path was judged there,
            // and a path whose state cannot be read tells nothing of a loss.
            Ok(position) if position.base.is_some() => {
                let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, &position.pallet);
                match std::fs::symlink_metadata(&path) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        found.entry(position.pallet).or_insert(RefDamage::Missing);
                    }
                    Err(error) => push_note(notes, format!(
                        "Whether {} stands on a lost pallet ref could not be told: the ref of pallet \"{}\" (\"{}\") could not be read: {}",
                        which, position.pallet, path.to_string_lossy(), error
                    )),
                }
            }
            Ok(_) => {}
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
    /// Whether the taint could not be read, which [`Tainted::read`] takes as torn.
    unread: bool,
}

impl Tainted {
    fn read(notes: &mut Vec<String>) -> Tainted {
        match taint_utils::read_taints(&forklift_root()) {
            Ok(state) => Tainted {
                hashes: state.recorded.iter().filter_map(|path| file_utils::hash_from_object_path(path)).collect(),
                torn: state.torn,
                unread: false,
            },
            Err(error) => {
                push_note(notes, format!("The durability taint could not be read, so every object salvaged from a damaged ref counts as possibly lost: {}", error));
                Tainted { hashes: BTreeSet::new(), torn: true, unread: true }
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
            Ok(Salvage::Unknown(reason) | Salvage::Untold(reason) | Salvage::Newer(reason)) => evidence.salvage_unknown = Some(reason),
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
    // A pack the store cannot list may hold a match, found or not, so no answer stands beside it.
    // What the packs cannot list is read once, at the first lookup, for every prefix the bytes
    // are looked up by; bytes that name no prefix never read it.
    let unlisted = std::cell::OnceCell::new();
    let matching = |prefix: &str| -> Result<Result<Vec<String>, Salvage>, String> {
        let packs = match unlisted.get() {
            Some(packs) => packs,
            None => {
                let read = crate::util::pack_utils::unlisted_packs()?;
                unlisted.get_or_init(|| read)
            }
        };
        if let Some(why) = packs.reason_for(prefix) {
            return Ok(Err(Salvage::Untold(format!(
                "what its bytes name cannot be looked up in full, since {}", why
            ))));
        }
        let mut matches = pallet_utils::object_hashes_with_prefix(prefix)?;
        matches.extend(tainted.hashes.iter().filter(|hash| hash.starts_with(prefix)).cloned());
        matches.sort();
        matches.dedup();
        Ok(Ok(matches))
    };

    if run >= HASH_LENGTH {
        return Ok(match matching(&String::from_utf8_lossy(&bytes[..HASH_LENGTH]))? {
            Ok(matches) => judge(matches, tainted),
            Err(untold) => untold,
        });
    }

    let mut matches = Vec::new();

    if bytes.len() >= HASH_LENGTH && bytes[..HASH_LENGTH].iter().filter(|byte| !is_hex(**byte)).count() == 1 {
        for digit in b"0123456789abcdef" {
            let mut hash = bytes[..HASH_LENGTH].to_vec();
            hash[run] = *digit;

            match matching(&String::from_utf8_lossy(&hash))? {
                Ok(found) => matches.extend(found),
                Err(untold) => return Ok(untold),
            }
        }
    }

    if matches.is_empty() && run >= MIN_SALVAGE_PREFIX {
        match matching(&String::from_utf8_lossy(&bytes[..run]))? {
            Ok(found) => matches = found,
            Err(untold) => return Ok(untold),
        }
    }

    Ok(judge(matches, tainted))
}

/// Whether a damaged ref's `bytes` name the one parcel heal rebuilds it at.
#[cfg(test)]
pub(crate) fn salvage_finds(bytes: &[u8]) -> bool {
    matches!(salvage(bytes, &Tainted::read(&mut Vec::new())), Ok(Salvage::Found(_)))
}

/// The salvage among `matches`: the one readable parcel, unless another parcel also matches, or
/// a match is tainted or cannot be read as any kind of object, which leaves the answer unknown
/// (the bytes then still name a head, only not which). A
/// match that reads as a tree, blob, recipe or chunk is not a parcel and drops out.
fn judge(matches: Vec<String>, tainted: &Tainted) -> Salvage {
    use crate::util::file_utils::ReadFault;

    let mut parcels = Vec::new();
    let mut unknown = Vec::new();
    let mut untold = Vec::new();
    let mut newer = Vec::new();

    for hash in matches {
        if tainted.covers(&hash) {
            unknown.push(hash);
            continue;
        }

        // Read as history reads it, so an absence a refill, a redaction or an unindexed pack may
        // account for is undecided. Bytes the store holds damaged, or decided are gone, are a
        // verdict on the object; a read that could not consult the store, or left it undecided,
        // says nothing of it.
        match object_utils::load_parcel_judged(&hash) {
            Ok(Ok(_)) => parcels.push(hash),
            Err((_, ReadFault::Untold)) => untold.push(hash),
            Err((_, ReadFault::Decided(_))) => unknown.push(hash),
            Err((_, ReadFault::Newer)) => newer.push(hash),
            // Bytes that read whole, told from their one parse: another kind of object drops
            // out, and bytes this build cannot parse are no damage, since the store verified them.
            Ok(Err(object_utils::NotAParcel::Other(_))) => {}
            Ok(Err(object_utils::NotAParcel::Newer(_))) => newer.push(hash),
        }
    }

    if parcels.len() > 1 {
        return Salvage::Ambiguous(parcels, !unknown.is_empty() || !untold.is_empty() || !newer.is_empty());
    }

    if let Some(hash) = newer.first() {
        return Salvage::Newer(format!(
            "what its bytes name may be object {}, which this Forklift cannot read: a newer Forklift may have written it", hash
        ));
    }

    if let Some(hash) = untold.first() {
        return Salvage::Untold(format!(
            "what its bytes name may be object {}, which the store could not be read for", hash
        ));
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
                meta: None,
            }),
            Choice::Rebuild(head, source) => {
                // A head whose history the trust check refuses is refused before anything is
                // written, so nothing of it is read back. The first pass leaves it to the second,
                // as it does a failed write.
                let preconditions = match trusted(&damaged.pallet, &head) {
                    Ok(preconditions) => preconditions,
                    Err(_) if pass == Pass::BeforeTaint => continue,
                    Err(error) => {
                        let reason = format!("the rebuild at {} was not written: {}", head, error);
                        report.refused.push(Refused { pallet: damaged.pallet.clone(), reason, candidates: listed(&evidence.candidates), meta: None });
                        continue;
                    }
                };

                match pallet_utils::repair_pallet_head(&damaged.pallet, &damaged.damage, &head, &preconditions) {
                    Ok(()) => report.rebuilt.push(Rebuilt { pallet: damaged.pallet.clone(), head, source }),
                    // Damaged tombstones refuse every ref write alike, and only the operator's
                    // repair mends them, so the refusal is the answer, with its own next step.
                    // What this pass already rebuilt stays reported.
                    Err(RefUpdateError::Failed(error)) if crate::util::tombstone_utils::is_damage(&error) => {
                        report.stopped = Some(error);
                        break;
                    }
                    Err(error) => match read_back(PalletNamespace::User, &damaged.pallet, &head) {
                        ReadBack::Landed => {
                            push_note(&mut report.notes, landed_despite(&damaged.pallet, &head, &error));
                            report.rebuilt.push(Rebuilt { pallet: damaged.pallet.clone(), head, source });
                        }
                        // The first pass leaves a failed write to the second, which reports it.
                        _ if pass == Pass::BeforeTaint => {}
                        read => report.refused.push(Refused {
                            pallet: damaged.pallet.clone(),
                            reason: unwritten_reason(&head, &error, &read),
                            candidates: listed(&evidence.candidates),
                            meta: None,
                        }),
                    },
                }
            }
        }
    }

    Ok(report)
}

/// The note for a rebuild of the ref of `pallet` (as it is written on the wire) at `head` that
/// landed although its write reported `error`: a failure after the ref's rename (its folder not
/// synced, a taint standing) leaves the ref written, and what the write reported stands beside it.
fn landed_despite(pallet: &str, head: &str, error: &RefUpdateError) -> String {
    format!("The ref of pallet \"{}\" was rebuilt at {}, though its write reported: {}", pallet, head, error)
}

/// The trust state a ref of `pallet` rebuilt or set at `head` is written under: in a warehouse
/// with an anchor, `head`'s history verifies against the anchor and office as they are now, as
/// an undone delete's does, since the written head is a local head no take verifies again.
fn trusted(pallet: &str, head: &str) -> Result<pallet_utils::RefPreconditions, String> {
    #[cfg(test)]
    if TRUSTED_REFUSED.with(|refused| refused.get()) {
        return Err("the trust check was refused for the test".to_string());
    }

    journal_utils::trust_preconditions_saying(
        &format!("The history of parcel {} does not pass this warehouse's trust check, so the ref of pallet \"{}\" is not set to it:", head, pallet),
        head,
        "Nothing was changed.",
    )
}

#[cfg(test)]
thread_local! {
    /// Makes [`trusted`] refuse, since no unit fixture holds an office whose trust check a
    /// rebuilt user head fails.
    static TRUSTED_REFUSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

    /// The file the removed ref's bytes were kept in; `None` for a meta ref that was lost, which
    /// held none.
    pub kept_ref: Option<String>,

    /// For a meta pallet, the file its removed copy's bytes were kept in; `None` when it had no
    /// copy, or the copy was not removed. Always `None` for a user pallet, which has no copy.
    pub kept_copy: Option<String>,

    /// Which namespace's ref it was, and what admitted its removal.
    #[serde(skip)]
    pub kind: RemovedKind,
}

/// Which namespace's ref `--remove-ref` removed, and what admitted its removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemovedKind {
    /// A user pallet's, removed only when its bytes do not read.
    User,

    /// A meta pallet's, removed on heal's verdict on its ref.
    Meta(Verdict),
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

    let kept = kept_path(pallet, None);

    pallet_utils::remove_damaged_ref(pallet, bytes, &kept).map_err(|error| error.to_string())?;

    Ok(Removed { pallet: pallet.to_string(), kept_ref: Some(kept.to_string_lossy().into_owned()), kept_copy: None, kind: RemovedKind::User })
}

/// The path under the removed-refs folder for bytes removed from `stem`'s file: the first that is
/// free, or, given `held`, that holds those bytes already, as a meta removal stopped before it
/// finished leaves it. A user pallet's numbered slot can be another pallet's own (`a.1` is
/// pallet `a`'s second and pallet `a.1`'s first), so a user removal takes a free one only. A meta
/// stem is `@` and a meta pallet's name, which no user pallet name can spell and which holds no
/// dot, so its slots and its copy's are its own (pinned by
/// `meta_kept_slots_are_no_other_pallets`).
fn kept_path(stem: &str, held: Option<&[u8]>) -> std::path::PathBuf {
    let folder = forklift_root().join(FOLDER_NAME_REMOVED_REFS);
    let stem = stem.replace('/', "%2F");
    (0..).map(|n| folder.join(match n { 0 => stem.clone(), n => format!("{}.{}", stem, n) }))
        // A slot whose state cannot be read is taken as free, and keeping the bytes there then
        // refuses it ([`pallet_utils`]'s keep reads it back), so it is never written over.
        .find(|path| std::fs::symlink_metadata(path).is_err() || held.is_some_and(|bytes| std::fs::read(path).is_ok_and(|kept| kept == bytes)))
        .expect("an unbounded range always yields a free name")
}

/// The step that lets a later heal judge meta ref `refused` again, among the refusals `all` of
/// the same pass: its removal, for a refusal of its own; for one that waits on another meta ref's
/// refusal, that ref's step; for one beside an unborn office, what brings the office back; for one
/// over what a newer Forklift may have written, the upgrade; for one heal could not read, settling
/// what it names; and otherwise heal again. A command, or sentences
/// when the way out is by hand.
pub fn meta_step(refused: &Refused, all: &[Refused]) -> String {
    let mut at = refused;
    // A cascade names the office or @redactions, whose own refusals do not cascade back.
    for _ in 0..3 {
        match at.meta.as_ref().map(|meta| &meta.owner) {
            // A copy path nothing can be kept from blocks the removal, so it is no step.
            Some(Owner::Own(_)) => return match at.meta.as_ref().is_some_and(|meta| pallet_utils::copy_blocks(&at.pallet[1..], &meta.copy)) {
                true => pallet_utils::blocked_copy_step(&at.pallet[1..], "run \"forklift heal\" again"),
                false => removal_step(&at.pallet),
            },
            Some(Owner::Cascade(cause)) => match all.iter().find(|other| other.pallet == *cause) {
                Some(cause) => at = cause,
                None => break,
            },
            Some(Owner::OfficeUnborn) => return crate::util::office_utils::office_unborn_step(),
            Some(Owner::Newer) => return newer_step(),
            Some(Owner::Trust(step)) => return step.clone(),
            Some(Owner::Unread(Some(step))) => return step.clone(),
            Some(Owner::Unread(None)) => return unread_step(&at.pallet),
            Some(Owner::Unwritten(Some(step))) => return step.clone(),
            Some(Owner::Unwritten(None)) => return unwritten_step(&at.pallet),
            Some(Owner::Again) | None => break,
        }
    }
    "forklift heal".to_string()
}

/// The way out of meta ref `wire`'s refusal heal could not read for: a rerun alone would read
/// the same, so the step is to settle what its refusal names first.
pub fn unread_step(wire: &str) -> String {
    format!("Settle what heal could not read for {}, as its refusal says, then run \"forklift heal\" again.", wire)
}

/// The way out of meta ref `wire`'s refusal heal could not write the rebuild for: a rerun meets a
/// lasting fault again, so the step is to settle why it failed first.
pub fn unwritten_step(wire: &str) -> String {
    format!("Settle why heal could not write the ref of {}, as its refusal says, then run \"forklift heal\" again.", wire)
}

/// The way out of a meta ref's refusal over what a newer Forklift may have written: a build that
/// reads it judges it.
pub fn newer_step() -> String {
    "Upgrade Forklift to a version that reads it, then run \"forklift heal\" again.".to_string()
}

/// The removal of meta ref `wire`: the step of a refusal of its own, where nothing blocks it.
pub fn removal_step(wire: &str) -> String {
    pallet_utils::meta_removal_step(&wire[1..])
}

/// `step`, a command or sentences from [`meta_step`], as a sentence of its own.
pub fn step_sentence(step: &str) -> String {
    match step.starts_with("forklift ") {
        true => format!("Run \"{}\".", step),
        false => step.to_string(),
    }
}

/// `forklift heal --remove-ref @<name>`: remove a meta pallet's lost or damaged ref and its copy,
/// keeping the bytes of each in a file of its own, when heal refused that ref for a reason of its
/// own. `judged` is what this run's meta pass before the taint recovery left unrebuilt, and
/// `unjudged` what it never judged. Admitted only with no taint record standing, and only while
/// the ref and its copy still stand as that pass judged them; afterwards the pallet reads unborn.
///
/// # Returns
/// * `Err((String, Option<Removed>))` - Why not, and what was removed before it stopped, if
///                                      anything: the ref, with its copy or not.
pub fn remove_damaged_meta_ref(wire: &str, judged: &[Refused], unjudged: &[String], notes: &mut Vec<String>) -> Result<Removed, (String, Option<Removed>)> {
    let mut partial = None;
    remove_meta_ref(wire, judged, unjudged, notes, &mut partial).map_err(|error| (error, partial))
}

/// [`remove_damaged_meta_ref`], what a removal that stopped had removed left in `partial`.
fn remove_meta_ref(wire: &str, judged: &[Refused], unjudged: &[String], notes: &mut Vec<String>, partial: &mut Option<Removed>) -> Result<Removed, String> {
    let name = wire.strip_prefix('@')
        .filter(|name| crate::util::meta_tree::META_PALLET_NAMES.contains(name))
        .ok_or_else(|| format!("\"{}\" is not a meta pallet this version of Forklift knows, so there is no ref of it to remove.", wire))?;

    // Under a taint, heal's verdict may rest on a parcel the recovery brings back, since meta
    // verification holds a parcel the store decided is absent against the ref as its own; and the
    // kept bytes' write would return the taint's error after landing.
    let first = "run \"forklift heal\" first, without --remove-ref, and remove it only if that refuses it naming the removal";
    match crate::util::taint_utils::read_taints(&forklift_root()) {
        Ok(taints) if taints.recorded.is_empty() && !taints.torn => {}
        Ok(_) => return Err(format!(
            "A durability taint stands, so heal's judgement of the ref of pallet \"{}\" may rest on a parcel its recovery brings back, and the ref was not removed: {}.",
            wire, first
        )),
        Err(error) => return Err(format!(
            "The durability taint could not be read ({}), so the ref of pallet \"{}\" was not removed: {}.",
            error.trim_end_matches('.'), wire, first
        )),
    }

    let Some(refused) = judged.iter().find(|refused| refused.pallet == wire) else {
        return Err(not_judged(name, wire, unjudged, notes));
    };
    let meta = refused.meta.as_ref().expect("a meta ref's refusal carries its verdict");

    let verdict = match &meta.owner {
        Owner::Own(verdict) => *verdict,
        Owner::Cascade(cause) => {
            let (said, step) = match judged.iter().find(|other| other.pallet == *cause) {
                Some(other) => (format!(": {}", other.reason.trim_end_matches('.')), meta_step(other, judged)),
                None => (String::new(), "forklift heal".to_string()),
            };
            return Err(format!(
                "The ref of pallet \"{}\" was not removed: heal refused it only because it refused the ref of pallet \"{}\"{}. \
                Once that is settled, a later heal judges this ref again. {}",
                wire, cause, said, step_sentence(&step)
            ));
        }
        Owner::OfficeUnborn => return Err(format!(
            "The ref of pallet \"{}\" was not removed: heal refused it only because the trust file holds this warehouse's anchor \
            while the office pallet is unborn, so nothing a meta pallet holds can be verified. Once the office stands, a later heal \
            judges this ref again. {}",
            wire, crate::util::office_utils::office_unborn_step()
        )),
        Owner::Newer => return Err(format!(
            "The ref of pallet \"{}\" was not removed: what heal needs to judge it is in a format this Forklift does not read ({}), \
            which is no verdict on the ref. {}",
            wire, refused.reason.trim_end_matches('.'), step_sentence(&newer_step())
        )),
        Owner::Trust(step) => return Err(format!(
            "The ref of pallet \"{}\" was not removed: the trust state it is verified against refuses ({}), which is no verdict on the ref. {}",
            wire, refused.reason.trim_end_matches('.'), step_sentence(step)
        )),
        Owner::Unread(step) => return Err(format!(
            "The ref of pallet \"{}\" was not removed: heal could not read what it needs to judge it ({}), which is no verdict on the ref. {}",
            wire, refused.reason.trim_end_matches('.'), step.as_deref().map_or_else(|| unread_step(wire), step_sentence)
        )),
        Owner::Unwritten(step) => return Err(format!(
            "The ref of pallet \"{}\" was not removed: heal chose its head but could not write it ({}), which is no verdict on the ref. {}",
            wire, refused.reason.trim_end_matches('.'), step.as_deref().map_or_else(|| unwritten_step(wire), step_sentence)
        )),
        Owner::Again => return Err(format!(
            "The ref of pallet \"{}\" was not removed: heal did not refuse it for a reason of its own ({}). Run \"forklift heal\" again.",
            wire, refused.reason.trim_end_matches('.')
        )),
    };

    let copy_path = pallet_utils::get_meta_copy_path(name);
    if pallet_utils::copy_blocks(name, &meta.copy) {
        return Err(format!(
            "Something that is not a file stands at the copy's path of pallet \"{}\", so it cannot be kept and the ref was not removed. {}",
            wire, pallet_utils::blocked_copy_step(name, "run \"forklift heal\" again")
        ));
    }

    // The ref's stem and the copy's never collide: neither is a prefix of the other's names.
    let kept_ref = match &meta.damage {
        RefDamage::Unreadable(bytes) => Some(kept_path(wire, Some(bytes))),
        _ => None,
    };
    let kept_copy = match &meta.copy {
        pallet_utils::MetaCopyState::Bytes(bytes) => Some(kept_path(&format!("{}.copy", wire), Some(bytes))),
        _ => None,
    };
    let said = |path: &Option<std::path::PathBuf>| path.as_ref().map(|path| path.to_string_lossy().into_owned());
    let removed = |kept_copy: Option<String>| Removed { pallet: wire.to_string(), kept_ref: said(&kept_ref), kept_copy, kind: RemovedKind::Meta(verdict) };

    match pallet_utils::remove_damaged_meta_ref(name, &meta.damage, &meta.copy, kept_ref.as_deref(), kept_copy.as_deref()) {
        Ok(()) => Ok(removed(said(&kept_copy))),
        Err((error, pallet_utils::MetaRemoved::Nothing)) => Err(error.to_string()),
        Err((error, pallet_utils::MetaRemoved::Ref)) => {
            *partial = Some(removed(None));
            Err(crate::error::append_to_message(error.to_string(), &format!(
                "The ref of pallet \"{}\" was removed, but its copy (\"{}\") still stands: run \"forklift heal\" again, which judges it.",
                wire, copy_path.to_string_lossy()
            )))
        }
        Err((error, pallet_utils::MetaRemoved::Both)) => {
            *partial = Some(removed(said(&kept_copy)));
            Err(error.to_string())
        }
    }
}

/// Why meta ref `wire` (`name`) is not removed when heal's pass before the taint recovery left
/// no refusal of it: it reads, heal never judged it, or heal could not read it.
fn not_judged(name: &str, wire: &str, unjudged: &[String], notes: &mut Vec<String>) -> String {
    if unjudged.iter().any(|other| other == wire) {
        return format!(
            "Heal stopped before judging the ref of pallet \"{}\", so it was not removed: once what stopped it is fixed, run \"forklift heal\" again.",
            wire
        );
    }
    // Only a note of this pallet's own says why it was not judged; another's says nothing of it.
    let mut skipped = Vec::new();
    let state = meta_states_skipping(&mut skipped).unwrap_or_default().into_iter().find(|state| state.name == name);
    let said: Vec<String> = skipped.into_iter().filter(|(skipped, _)| skipped == name).map(|(_, note)| note).collect();
    match state {
        Some(state) if state.damage().is_none() => format!("The ref of pallet \"{}\" reads, so there is nothing to remove.", wire),
        Some(_) => format!(
            "The ref of pallet \"{}\" was damaged after heal judged it, so it was not removed: run \"forklift heal\" again.",
            wire
        ),
        None if said.is_empty() => format!("Pallet \"{}\" has neither a ref nor a copy, so it reads as unborn and there is nothing to remove.", wire),
        None => {
            // Heal's own pass may have noted it already.
            for note in said {
                push_note(notes, note);
            }
            format!(
                "Heal did not judge the ref of pallet \"{}\", for what the note says, so it was not removed: what cannot be read cannot be kept. \
                Settle what the note names, then run the command again.",
                wire
            )
        }
    }
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
    let mut skipped = Vec::new();
    let states = meta_states_skipping(&mut skipped);
    for (_, note) in skipped {
        push_note(notes, note);
    }
    states
}

/// [`meta_states`], each pallet left out in `skipped` with its note, by name.
fn meta_states_skipping(skipped: &mut Vec<(String, String)>) -> Result<Vec<MetaState>, String> {
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
                skipped.push((name.clone(), not_a_file_note(&meta_wire(&name), &path)));
                continue;
            }
            Err(error) => {
                skipped.push((name.clone(), format!("The ref of pallet \"{}\" could not be read to tell whether it is damaged: {}", meta_wire(&name), error)));
                continue;
            }
        };

        match pallet_utils::read_meta_copy_state(&name) {
            Ok(pallet_utils::MetaCopyState::Other) if reference.is_none() => skipped.push((name.clone(), format!(
                "Something that is not a file stands at the copy of pallet \"{}\"'s ref (\"{}\"), which has no ref: it names no head, so heal did not judge that ref.",
                meta_wire(&name), pallet_utils::get_meta_copy_path(&name).to_string_lossy()
            ))),
            Ok(copy) => states.push(MetaState { name, reference, copy }),
            Err(error) => skipped.push((name.clone(), format!(
                "The copy of pallet \"{}\"'s ref could not be read, so heal did not judge that ref: {}", meta_wire(&name), error
            ))),
        }
    }

    Ok(states)
}

/// Whether `head` is `ancestor` or descends from it along first parents. The error says what the
/// failed parcel read tells of that history.
fn first_parent_descends(head: &str, ancestor: &str) -> Result<bool, (String, crate::util::file_utils::ReadFault)> {
    let mut at = head.to_string();
    let mut seen = BTreeSet::new();

    loop {
        if at == ancestor {
            return Ok(true);
        }

        if !seen.insert(at.clone()) {
            return Ok(false);
        }

        match object_utils::load_parcel_faulted(&at)?.parents.first() {
            Some(parent) => at = parent.clone(),
            None => return Ok(false),
        }
    }
}

const FROM_COPY: &str = "the ref's copy";

/// A meta ref's refusal before any write: why, the heads found, and whose refusal it is.
type MetaRefusal = (String, Vec<Candidate>, Owner);

/// The head a damaged meta ref is rebuilt at, and where it was found, or why none is.
///
/// A copy that differs is taken as ahead of the ref because a forward write moves the copy first.
/// Undo's put-back moves a ref backward and writes the copy second, so one interrupted between
/// the two leaves the copy at the head it was undoing, and this would rebuild there. Only a
/// journaled command's refs are put back, and none of `journal_op`'s commands (stack, consolidate,
/// pick, shift) writes a meta ref: `git grep -l -E 'update_meta_pallet_head|PalletNamespace::Meta'`
/// over their command and util modules finds none. A ref ahead of its copy (a write whose copy
/// was not mended, a copy restored from an older backup) is rebuilt at what its own bytes name.
fn choose_meta(state: &MetaState, tainted: &Tainted) -> Result<(String, &'static str), MetaRefusal> {
    let copy = state.copy_head();
    let mut candidates: Vec<Candidate> = copy.iter()
        .map(|head| Candidate { head: head.clone(), source: FROM_COPY.to_string() })
        .collect();

    let salvaged = match &state.reference {
        Some(bytes) => salvage(bytes, tainted),
        None => Ok(Salvage::Nothing),
    };
    // A lookup that failed says nothing of the ref.
    let salvaged = salvaged.map_err(|error| (
        format!("what its own bytes name could not be looked up: {}", error.trim_end_matches('.')), candidates.clone(), Owner::Unread(None)
    ))?;

    // Whose a salvage that names no head heal takes is: the ref's own where the store's answer is
    // a verdict (nothing, several parcels, a match it decided is unknown), a newer build's for a
    // match this build cannot parse, unread where the store said nothing (a match or a pack it
    // could not read).
    match salvaged {
        Salvage::Nothing => match copy {
            Some(head) => Ok((head, FROM_COPY)),
            None => Err(("neither its copy nor its own bytes name a head".to_string(), candidates, Owner::Own(Verdict::NoHead))),
        },
        Salvage::Found(salvaged) => {
            candidates.push(Candidate { head: salvaged.clone(), source: Source::Salvage.describe() });

            match copy {
                None => Ok((salvaged, "what the ref's own bytes still name")),
                Some(copy) if copy == salvaged => Ok((copy, FROM_COPY)),
                // A copy a forward write left ahead of its ref is in this shape, and a ref whose
                // copy was not mended in the other; one an interrupted re-genesis or a hand edit
                // left need not be either, and then nothing proves either the newer.
                Some(copy) => {
                    // History the store decided is damaged or gone proves neither the newer; a
                    // read that said nothing of it is no verdict.
                    let untold = |(descendant, ancestor): (&str, &str), (error, fault): (String, crate::util::file_utils::ReadFault), candidates| (format!(
                        "whether {} descends from {} could not be told: {}",
                        descendant, ancestor, error.trim_end_matches('.')
                    ), candidates, match fault {
                        crate::util::file_utils::ReadFault::Decided(decided) => Owner::Own(Verdict::Object(decided)),
                        crate::util::file_utils::ReadFault::Newer => Owner::Newer,
                        crate::util::file_utils::ReadFault::Untold => Owner::Unread(None),
                    });

                    match first_parent_descends(&copy, &salvaged) {
                        Ok(true) => Ok((copy, FROM_COPY)),
                        Ok(false) => match first_parent_descends(&salvaged, &copy) {
                            Ok(true) => Ok((salvaged, "what the ref's own bytes still name")),
                            Ok(false) => Err((format!(
                                "its copy names {} and its own bytes name {}, and neither descends along first parents from the other, so nothing proves either the newer",
                                copy, salvaged
                            ), candidates, Owner::Own(Verdict::Diverged))),
                            Err(error) => Err(untold((&format!("what its own bytes name, {},", salvaged), &format!("its copy's head {}", copy)), error, candidates)),
                        },
                        Err(error) => Err(untold((&format!("its copy's head {}", copy), &format!("{}, what its own bytes name,", salvaged)), error, candidates)),
                    }
                }
            }
        }
        Salvage::Ambiguous(parcels, _) => {
            candidates.extend(parcels.into_iter().map(|head| Candidate { head, source: Source::Salvage.describe() }));
            Err(("its own bytes name one of several parcels, not which".to_string(), candidates, Owner::Own(Verdict::AmbiguousHead)))
        }
        Salvage::Unknown(why) => Err((why, candidates, Owner::Own(Verdict::UnknownHead))),
        Salvage::Untold(why) => Err((why, candidates, Owner::Unread(None))),
        Salvage::Newer(why) => Err((why, candidates, Owner::Newer)),
    }
}

/// Verify `head` as the whole history of meta pallet `name`, against the trust anchor and the
/// office as they are now, and return the preconditions that pin both for the write.
///
/// # Returns
/// * `Err((reason, owner))` - Why not, and whose refusal that is: the ref's own when `head` fails
///                            verification, [`Owner::OfficeUnborn`] when the office is unborn
///                            beside an anchor, which refuses before `head` is read, the trust
///                            read's owner ([`trust_read_owner`]) when the anchor could not be
///                            read, else as [`verify_meta_against`] says; [`Owner::Unread`] when
///                            the office could not be read.
fn verify_meta_head(name: &str, head: &str) -> Result<pallet_utils::RefPreconditions, (String, Owner)> {
    let anchor = crate::util::office_utils::read_trust_anchor_judged(crate::util::office_utils::TrustReadMode::Checked)
        .map_err(|error| { let owner = trust_read_owner(&error); (String::from(error), owner) })?;
    let office_head = pallet_utils::get_meta_pallet_head(crate::util::office_utils::OFFICE_PALLET_NAME).map_err(|error| (error, Owner::Unread(None)))?;

    // Said here rather than by the verifier, whose step (lift the office) is no step in a store
    // that lacks its office.
    if anchor.is_some() && office_head.is_none() {
        return Err(("the trust file holds this warehouse's anchor, but the office pallet is unborn, so nothing a meta pallet holds can be verified".to_string(), Owner::OfficeUnborn));
    }

    verify_meta_against(name, head, anchor.as_ref().map(|(anchor, _)| anchor), office_head.as_deref())?;

    Ok(pallet_utils::RefPreconditions {
        office: pallet_utils::OfficePrecondition::At(office_head),
        anchor: match anchor {
            Some((_, bytes)) => pallet_utils::AnchorPrecondition::Bytes(bytes),
            None => pallet_utils::AnchorPrecondition::Absent,
        },
        pallets: pallet_utils::PalletsPrecondition::NotConsumed,
    })
}

/// Whose a refusal of a meta ref other than the office is, when heal chose no head for it for a
/// reason `owner` says: a reason of its own is still the office's while the office is refused
/// (`blocking`) or unborn beside the trust file's anchor, since its removal would then be judged
/// against no office.
fn choice_owner(owner: Owner, blocking: &[String]) -> Owner {
    if !matches!(owner, Owner::Own(_)) {
        return owner;
    }
    if let Some(cause) = blocking.first() {
        return Owner::Cascade(cause.clone());
    }
    let anchor = crate::util::office_utils::read_trust_anchor_judged(crate::util::office_utils::TrustReadMode::Checked);
    let office = pallet_utils::get_meta_pallet_head(crate::util::office_utils::OFFICE_PALLET_NAME);
    match (anchor, office) {
        (Err(error), _) => trust_read_owner(&error),
        (Ok(Some(_)), Ok(None)) => Owner::OfficeUnborn,
        (Ok(_), Ok(_)) => owner,
        (Ok(_), Err(_)) => Owner::Unread(None),
    }
}

/// Whose a meta ref's refusal is when the trust anchor it is verified against could not be read:
/// the trust state's when it reads and refuses (a lost anchor, a damaged trust file), whose
/// refusal names its own ways out, so the step is a rerun when heal restores the trust file from
/// its copy and otherwise taking one of them; a newer build's; or unread.
fn trust_read_owner(error: &crate::util::office_utils::TrustReadError) -> Owner {
    match error {
        crate::util::office_utils::TrustReadError::Refused(_) => Owner::Trust(match trust_remedy().0 {
            step if step.is_empty() => TRUST_REFUSED_STEP.to_string(),
            step => step,
        }),
        crate::util::office_utils::TrustReadError::Newer(_) => Owner::Newer,
        crate::util::office_utils::TrustReadError::Unread(_) => Owner::Unread(None),
    }
}

/// Verify `head` as the whole history of meta pallet `name`, against `anchor` and the office at
/// `office_head` (`None`: no anchor, or an unborn office).
///
/// # Returns
/// * `Err((reason, owner))` - The ref's own refusal when the verdict is on `head`'s history (a rule
///                            it breaks, or a parcel of it the store decided is absent or holds
///                            damaged); [`Owner::Newer`] when what it needs is in a format this
///                            build does not know; [`Owner::OfficeUnborn`] or [`Owner::Trust`]
///                            when the trust state it is checked against refuses, read whole;
///                            [`Owner::Unread`] when the refusal says nothing of it.
fn verify_meta_against(name: &str, head: &str, anchor: Option<&crate::util::office_utils::TrustAnchor>, office_head: Option<&str>) -> Result<(), (String, Owner)> {
    use crate::util::meta_tree::{MetaCheckError, TrustGap};

    let Some(rules) = crate::util::meta_tree::meta_rules(name) else {
        return Err(("it is not a meta pallet heal has rules to verify against".to_string(), Owner::Own(Verdict::NoRules)));
    };

    crate::util::meta_tree::verify_meta_update(name, rules, crate::util::meta_tree::Admission::Audit, anchor, office_head, None, head)
        .map(|_| ())
        .map_err(|error| {
            let owner = match error {
                MetaCheckError::Verdict(verdict, _) => Owner::Own(verdict),
                MetaCheckError::Newer(_) => Owner::Newer,
                MetaCheckError::Trust(_, TrustGap::OfficeUnborn) => Owner::OfficeUnborn,
                MetaCheckError::Trust(_, TrustGap::AnchorLost) => Owner::Trust(trust_step(TrustGap::AnchorLost)),
                MetaCheckError::Trust(_, TrustGap::ChainRefused) => Owner::Trust(trust_step(TrustGap::ChainRefused)),
                MetaCheckError::Untold(_) => Owner::Unread(None),
            };
            (format!("parcel {} does not pass verification: {}", head, error.message().trim_end_matches('.')), owner)
        })
}

/// What settles the trust state `gap` that refuses a meta ref: for an unborn office, what brings
/// it back; otherwise a rerun, when heal restores the trust file from its copy; failing that, for
/// a lost anchor the ways out that hold on this root, and for a chain that does not verify,
/// settling why, which nothing here can tell.
fn trust_step(gap: crate::util::meta_tree::TrustGap) -> String {
    use crate::util::meta_tree::TrustGap;

    if gap == TrustGap::OfficeUnborn {
        return crate::util::office_utils::office_unborn_step();
    }
    let (step, ways_out) = trust_remedy();
    match (step.is_empty(), gap) {
        (false, _) => step,
        (true, TrustGap::AnchorLost) => ways_out.to_string(),
        (true, TrustGap::ChainRefused | TrustGap::OfficeUnborn) => {
            "Settle why the office pallet does not verify against the trust file's anchor, as the refusal says, then run \"forklift heal\" again.".to_string()
        }
    }
}

thread_local! {
    /// The trust state's remedy while a [`RemedyScope`] stands: `Some(None)` until first read.
    static TRUST_REMEDY: std::cell::RefCell<Option<Option<(String, &'static str)>>> = const { std::cell::RefCell::new(None) };
}

/// [`office_utils::trust_remedy`](crate::util::office_utils::trust_remedy), which rereads both
/// trust files and verifies their anchors against the office chain: read once while a
/// [`RemedyScope`] stands, at the first refusal that needs it, and afresh outside one.
fn trust_remedy() -> (String, &'static str) {
    if let Some(remedy) = TRUST_REMEDY.with(|cell| cell.borrow().clone().flatten()) {
        return remedy;
    }
    let remedy = crate::util::office_utils::trust_remedy();
    TRUST_REMEDY.with(|cell| if let Some(slot) = cell.borrow_mut().as_mut() {
        *slot = Some(remedy.clone());
    });
    remedy
}

/// A stretch of heal that writes neither trust file nor the office ref, so the trust state's
/// remedy holds across it and is read once ([`trust_remedy`]).
struct RemedyScope;

impl RemedyScope {
    fn enter() -> RemedyScope {
        TRUST_REMEDY.with(|cell| *cell.borrow_mut() = Some(None));
        RemedyScope
    }
}

impl Drop for RemedyScope {
    fn drop(&mut self) {
        TRUST_REMEDY.with(|cell| *cell.borrow_mut() = None);
    }
}

/// The step of a meta ref refused over a trust file that reads and refuses, whose refusal names
/// the ways out: they are in the reason, so the step does not restate them.
const TRUST_REFUSED_STEP: &str = "Take one of the ways out the trust file's refusal names, then run \"forklift heal\" again.";

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
    /// What [`Pass::BeforeTaint`] deferred before it stopped: the office, when it left that.
    pub deferred: Vec<Refused>,
    /// What [`Pass::AfterTaint`] refused before it stopped: the office, when it refused that.
    pub refused: Vec<Refused>,
    /// Every other lost or damaged meta ref, as `@<name>`, which the pass stopped before judging.
    pub unjudged: Vec<String>,
    /// The deferred refs the taint held ([`Repair::held`]).
    pub held: Option<TaintHold>,
    /// What the pass noted before it stopped, the office step's among them.
    pub notes: Vec<String>,
}

/// The meta refs [`Pass::BeforeTaint`] deferred only because a taint counts every object as one it
/// may have lost, and why it does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaintHold {
    /// Whether the taint could not be read at all, rather than read as torn.
    pub unread: bool,
    /// The refs, as `@<name>`.
    pub refs: Vec<String>,
}

fn is_office(state: &MetaState) -> bool {
    state.name == crate::util::office_utils::OFFICE_PALLET_NAME
}

fn is_redactions(state: &MetaState) -> bool {
    state.name == crate::util::redaction_utils::REDACTIONS_PALLET_NAME
}

/// Run one repair pass over every meta ref, in three steps:
/// 1. A lost or damaged office ref is rebuilt from its copy or its own bytes, at a head whose
///    chain verifies against the anchor of the trust file or, failing that, of its copy. The
///    write is pinned to both files as they were judged, not to the office, whose ref is what is
///    being rebuilt. Beside a lost or damaged `@redactions` ref, the head that ref is to be
///    rebuilt at is chosen and verified first, against that anchor and the office head chosen,
///    and the office's write is checked against it; with no such head the office is refused.
/// 2. The trust file and its copy are repaired ([`repair_trust`]), reading the office as step 1
///    left it, so a trust file lost beside a lost office is restored once the office stands.
/// 3. A lost or damaged `@redactions` ref is rebuilt first, while the office reads, verified
///    against the anchor and the office as step 2 left them, and checked against the head it is
///    rebuilt at. Then every meta ref that reads has a lost or differing copy set to it, and
///    every other lost or damaged one is rebuilt the same way, checked against the redactions
///    as `@redactions` then reads. One beside an office or a `@redactions` still damaged is
///    refused, since each is verified against the office and checked against the redactions.
///
/// A head chosen for `@redactions` stands in for what its ref would hold: a write checked
/// against it is refused as moved once that ref or its copy no longer stands as heal judged it
/// ([`JudgedRedactions`]).
///
/// [`JudgedRedactions`]: crate::util::redaction_utils::JudgedRedactions
///
/// [`Pass::BeforeTaint`] refuses nothing: what it would refuse, a write that failed among it,
/// it lists as deferred ([`Repair::deferred`]) for [`Pass::AfterTaint`], which judges again.
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
    let redactions = states.iter().find(|state| is_redactions(state) && state.damage().is_some());
    let office = states.iter().find(|state| is_office(state) && state.damage().is_some());
    let mut judged = None;

    if let Some(state) = office {
        let tainted = tainted.get_or_insert_with(|| Tainted::read(&mut report.notes));
        judged = rebuild_office(state, redactions, tainted, pass, &mut report);
    }

    let trust = crate::util::office_utils::repair_trust();
    if trust.is_ok() && report.stopped.is_none() {
        repair_other_meta(pass, office.is_some(), judged, &mut tainted, &mut report);
    }
    // A pass that stopped, at the trust repair or inside step 3, names what it never judged.
    if trust.is_err() || report.stopped.is_some() {
        report.unjudged = unjudged_in(&mut report);
    }
    report.held = tainted.as_ref().filter(|tainted| tainted.torn).and_then(|tainted| held_in(tainted, &mut report));

    match trust {
        Ok(trust) => Ok(MetaRepair { repair: report, trust }),
        Err(error) => {
            let mut notes = std::mem::take(&mut report.notes);
            if let Some(stopped) = report.stopped.take() {
                notes.push(format!("The office pallet's ref was not rebuilt: {}", stopped.trim_end_matches('.')));
            }
            Err(TrustStopped {
                error, rebuilt: std::mem::take(&mut report.rebuilt), deferred: std::mem::take(&mut report.deferred),
                refused: std::mem::take(&mut report.refused), unjudged: std::mem::take(&mut report.unjudged), held: report.held.take(), notes,
            })
        }
    }
}

/// The lost or damaged meta refs, as `@<name>`, that `report` neither rebuilt, refused nor
/// deferred: what a pass that stopped never judged. Read again, since step 3 judges from its own
/// read; what that read could not judge is noted into `report`.
fn unjudged_in(report: &mut Repair) -> Vec<String> {
    let mut notes = Vec::new();
    let states = meta_states(&mut notes).unwrap_or_default();
    for note in notes {
        push_note(&mut report.notes, note);
    }
    let judged = |wire: &str| report.rebuilt.iter().any(|rebuilt| rebuilt.pallet == wire)
        || report.refused.iter().chain(&report.deferred).any(|refused| refused.pallet == wire);
    states.iter().filter(|state| state.damage().is_some()).map(MetaState::wire).filter(|wire| !judged(wire)).collect()
}

/// The refs `report` deferred whose own bytes, read again, name only objects `tainted` counts as
/// possibly lost; `None` when there are none. Beside a torn taint every object counts so.
fn held_in(tainted: &Tainted, report: &mut Repair) -> Option<TaintHold> {
    if report.deferred.is_empty() {
        return None;
    }
    let mut notes = Vec::new();
    let states = meta_states(&mut notes).unwrap_or_default();
    for note in notes {
        push_note(&mut report.notes, note);
    }
    let held = |wire: &str| states.iter().find(|state| state.wire() == wire)
        .and_then(|state| state.reference.as_deref())
        .is_some_and(|bytes| matches!(salvage(bytes, tainted), Ok(Salvage::Unknown(_))));
    let refs: Vec<String> = report.deferred.iter().map(|deferred| deferred.pallet.clone()).filter(|wire| held(wire)).collect();
    (!refs.is_empty()).then_some(TaintHold { unread: tainted.unread, refs })
}

/// What step 1 of [`repair_meta`] judged of a damaged `@redactions` beside a damaged office, for
/// step 3, which writes it.
enum RedactionsJudged {
    /// The head chosen and verified, found at the source named, which the office's write was
    /// checked against: step 3 writes this head, or none.
    Verified(crate::util::redaction_utils::JudgedRedactions, &'static str),

    /// Why no head was chosen and verified, which refused the office, what names a head, and
    /// whose refusal that is.
    Refused(String, Vec<Candidate>, Owner),
}

/// Step 1 of [`repair_meta`]: judge the damaged office ref `state` and write it, checked against
/// the head chosen for `redactions`, a damaged `@redactions` ref beside it.
///
/// # Returns
/// * `Some(RedactionsJudged)` - What was judged of `redactions`, when the office's own judgment
///                              got that far.
fn rebuild_office(state: &MetaState, redactions: Option<&MetaState>, tainted: &Tainted, pass: Pass, report: &mut Repair) -> Option<RedactionsJudged> {
    let (head, source) = match choose_meta(state, tainted) {
        Ok(chosen) => chosen,
        Err((reason, candidates, owner)) => {
            refuse_in(report, state, pass, reason, candidates, owner);
            return None;
        }
    };
    let candidates = || vec![Candidate { head: head.clone(), source: source.to_string() }];

    // The trust file's anchor first, so a copy's is used only where the trust file's does not
    // serve; the write is pinned to both files as they were judged.
    let (anchor, judged_anchor) = match crate::util::office_utils::office_rebuild_anchor(&head) {
        Ok(anchor) => anchor,
        // No anchor verifies the office's own candidate, a newer version may judge it, or what
        // would tell could not be read.
        Err((reason, refusal)) => {
            let owner = match refusal {
                crate::util::office_utils::AnchorRefusal::Own(verdict) => Owner::Own(verdict),
                crate::util::office_utils::AnchorRefusal::Newer => Owner::Newer,
                crate::util::office_utils::AnchorRefusal::Unread => Owner::Unread(None),
            };
            refuse_in(report, state, pass, reason, candidates(), owner);
            return None;
        }
    };

    // Against the anchor and the office head judged here: the office's ref reads as damaged.
    let judged = match redactions {
        None => None,
        Some(redactions) => {
            let refused = |why: &str, unjudged: MetaRefusal| {
                (format!(
                    "it is checked against the redactions, and the ref of pallet \"{}\" is damaged too, {}: set that first",
                    redactions.wire(), why
                ), RedactionsJudged::Refused(unjudged.0, unjudged.1, unjudged.2))
            };
            let outcome = match choose_meta(redactions, tainted) {
                Err(unjudged) => Err(refused("and heal chose no head for it", unjudged)),
                Ok((chosen, chosen_source)) => {
                    match verify_meta_against(crate::util::redaction_utils::REDACTIONS_PALLET_NAME, &chosen, Some(&judged_anchor), Some(&head)) {
                        Ok(()) => Ok((judged_redactions(redactions, chosen), chosen_source)),
                        Err((reason, owner)) => Err(refused(
                            "with no head heal can verify",
                            (reason, vec![Candidate { head: chosen, source: chosen_source.to_string() }], owner),
                        )),
                    }
                }
            };
            match outcome {
                Ok(judged) => Some(judged),
                Err((reason, unjudged)) => {
                    refuse_in(report, state, pass, reason, candidates(), Owner::Cascade(redactions.wire()));
                    return Some(unjudged);
                }
            }
        }
    };

    let preconditions = pallet_utils::RefPreconditions {
        office: pallet_utils::OfficePrecondition::NotConsumed,
        anchor,
        pallets: pallet_utils::PalletsPrecondition::NotConsumed,
    };

    write_meta_rebuild(state, head, source, &preconditions, judged.as_ref().map(|(judged, _)| judged), pass, report);
    judged.map(|(judged, source)| RedactionsJudged::Verified(judged, source))
}

/// Damaged `@redactions` ref `state` as heal judged it, to be rebuilt at `head`.
fn judged_redactions(state: &MetaState, head: String) -> crate::util::redaction_utils::JudgedRedactions {
    crate::util::redaction_utils::JudgedRedactions {
        head,
        damage: state.damage().expect("only a damaged @redactions is judged"),
        copy: state.copy.clone(),
    }
}

/// The first step a frame of `reason` names other than heal itself, the command that refused.
fn lifted_step(reason: &str) -> Option<String> {
    crate::error::frames_in(reason)
        .map(|(_, _, step)| step.trim())
        .find(|step| !step.is_empty() && *step != "forklift heal")
        .map(str::to_string)
}

/// Refuse meta ref `state` for `reason`, listing `candidates`: in [`Pass::AfterTaint`]; the pass
/// before the taint recovery defers it to that pass instead.
fn refuse_in(report: &mut Repair, state: &MetaState, pass: Pass, reason: String, candidates: Vec<Candidate>, owner: Owner) {
    // The refusal's step is its owner's (meta_step), so every frame keeps only its message. An
    // unread one's way out is the first step a frame of its reason names other than heal itself
    // (the very command that refused), carried as the owner's step; so is an unwritten one's.
    let owner = match owner {
        Owner::Unread(None) => Owner::Unread(lifted_step(&reason)),
        Owner::Unwritten(None) => Owner::Unwritten(lifted_step(&reason)),
        owner => owner,
    };
    let meta = MetaVerdict { owner, damage: state.damage().expect("only a damaged meta ref is refused"), copy: state.copy.clone() };
    let reason = crate::error::frames_without_steps(&reason);
    let refused = Refused { pallet: state.wire(), reason, candidates, meta: Some(meta) };
    match pass {
        Pass::AfterTaint => report.refused.push(refused),
        Pass::BeforeTaint => report.deferred.push(refused),
    }
}

/// What the ref of a pallet heal wrote at a head reads back as. A write can land and still report
/// a failure (under a standing taint, its rename goes through and the taint is reported), so the
/// ref, read as it stands, decides whether it landed.
enum ReadBack {
    /// The ref holds the head heal wrote.
    Landed,
    /// The ref reads, and holds no such head: missing, damaged, at another head, or not a file.
    Elsewhere,
    /// The ref could not be read: no answer on whether the write landed.
    Unread(String),
}

impl ReadBack {
    fn unread(&self) -> Option<String> {
        match self {
            ReadBack::Unread(why) => Some(why.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Makes [`read_back`] answer [`ReadBack::Unread`], since no fixture fails the read of a ref
    /// heal could judge right after its write failed.
    static READ_BACK_UNREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read back the ref of pallet `name` after heal wrote it at `head`.
fn read_back(namespace: PalletNamespace, name: &str, head: &str) -> ReadBack {
    #[cfg(test)]
    if READ_BACK_UNREAD.with(|unread| unread.get()) {
        return ReadBack::Unread("the read was refused for the test".to_string());
    }

    match pallet_utils::read_file_state(&pallet_utils::get_pallet_ref_path(namespace, name)) {
        Ok(pallet_utils::MetaCopyState::Bytes(bytes)) if pallet_utils::parse_ref_bytes(&bytes).as_deref() == Some(head) => ReadBack::Landed,
        Ok(_) => ReadBack::Elsewhere,
        Err(why) => ReadBack::Unread(why),
    }
}

/// Why the rebuild at `head` whose write refused with `error` is refused, as its ref read back
/// says: a write that failed may have landed, so one whose ref could not be read back is not
/// said to be unwritten. A precondition that moved refused before anything was written.
fn unwritten_reason(head: &str, error: &RefUpdateError, read: &ReadBack) -> String {
    match (error, read) {
        (RefUpdateError::Failed(_), ReadBack::Unread(why)) => format!(
            "the rebuild at {} may have been written: the write reported {}, and its ref could not be read back: {}",
            head, error, why
        ),
        _ => format!("the rebuild at {} was not written: {}", head, error),
    }
}

/// Write the rebuild of damaged meta ref `state` at `head`, found at `source`, and report it,
/// checked against the head `judged` chose for a damaged `@redactions` when there is one.
///
/// # Returns
/// * `true` - If the pass stops here (`report.stopped` says why).
fn write_meta_rebuild(state: &MetaState,
                      head: String,
                      source: &str,
                      preconditions: &pallet_utils::RefPreconditions,
                      judged: Option<&crate::util::redaction_utils::JudgedRedactions>,
                      pass: Pass,
                      report: &mut Repair) -> bool {
    let damage = state.damage().expect("only a damaged ref is rebuilt");
    let written = pallet_utils::repair_meta_pallet_head(&state.name, &damage, &state.copy, &head, preconditions, judged);

    let read = read_back(PalletNamespace::Meta, &state.name, &head);

    let (reason, owner) = match written {
        // As in the user pass: the refusal is the answer, and what this pass rebuilt stays
        // reported.
        Err(RefUpdateError::Failed(error)) if crate::util::tombstone_utils::is_damage(&error) => {
            report.stopped = Some(error);
            return true;
        }
        // As in the user pass: what the write reported stands beside the ref it wrote.
        _ if matches!(read, ReadBack::Landed) => {
            if let Err(error) = &written {
                push_note(&mut report.notes, landed_despite(&state.wire(), &head, error));
            }
            report.rebuilt.push(Rebuilt { pallet: state.wire(), head, source: source.to_string() });
            return false;
        }
        // A precondition that moved is heal's own race: a later heal judges the ref afresh.
        Err(error @ (RefUpdateError::Moved(_) | RefUpdateError::OfficeMoved { .. } | RefUpdateError::AnchorMoved
            | RefUpdateError::PalletsMoved { .. } | RefUpdateError::WitnessMoved { .. })) =>
            (format!("the rebuild at {} was not written: {}", head, error), Owner::Again),
        // Any other refusal may meet heal the same way on every rerun (a full disk, a folder it
        // may not write, a head the write itself refuses), so its way out is to settle why,
        // which a passing fault needs nothing for.
        Err(error) => (unwritten_reason(&head, &error, &read), Owner::Unwritten(None)),
        // Written, but its ref could not be read back: no answer on whether it landed.
        Ok(()) if matches!(read, ReadBack::Unread(_)) => (
            format!("the rebuild at {} was written, but its ref could not be read back: {}", head, read.unread().unwrap_or_default()),
            Owner::Unwritten(None),
        ),
        // Written, and the ref reads at another head: it moved under heal.
        Ok(()) => (format!("the rebuild at {} did not read back", head), Owner::Again),
    };

    refuse_in(report, state, pass, reason, vec![Candidate { head, source: source.to_string() }], owner);
    false
}

/// Step 3 of [`repair_meta`], over the meta refs as the steps before it left them. The office is
/// skipped when step 1 judged it (`office_judged`), which reported it; `judged` is what step 1
/// judged of a damaged `@redactions` beside it.
fn repair_other_meta(pass: Pass, office_judged: bool, judged: Option<RedactionsJudged>, tainted: &mut Option<Tainted>, report: &mut Repair) {
    // This runs after the trust repair and writes neither trust file nor the office ref (an
    // office met here is refused), so the trust state's remedy holds throughout.
    let _remedy = RemedyScope::enter();
    let states = meta_states(&mut report.notes).unwrap_or_default();

    if states.iter().any(|state| state.damage().is_some()) && tainted.is_none() {
        *tainted = Some(Tainted::read(&mut report.notes));
    }

    let mut blocking: Vec<String> = states.iter()
        .filter(|state| state.damage().is_some() && is_office(state))
        .map(MetaState::wire)
        .collect();

    // Every other meta ref's rebuild is checked against the redactions, so @redactions goes first.
    if let Some(state) = states.iter().find(|state| state.damage().is_some() && is_redactions(state)) {
        let tainted = tainted.as_ref().expect("read when a ref is damaged");

        if !rebuild_redactions(state, &blocking, judged, tainted, pass, report) {
            blocking.push(state.wire());
        }

        if report.stopped.is_some() {
            return;
        }
    }

    for state in &states {
        if state.damage().is_none() {
            if let Some(head) = state.reference.as_deref().and_then(pallet_utils::parse_ref_bytes) {
                if state.copy != pallet_utils::MetaCopyState::Bytes(state.reference.clone().unwrap_or_default()) {
                    mend_copy(state, &head, pass, &mut report.notes);
                }
            }
            continue;
        }

        // Rebuilt above, or refused there.
        if is_redactions(state) || (office_judged && is_office(state)) {
            continue;
        }

        let mut refuse = |reason: String, candidates: Vec<Candidate>, owner: Owner| refuse_in(report, state, pass, reason, candidates, owner);

        let choice = choose_meta(state, tainted.as_ref().expect("read when a ref is damaged"));

        // Step 1 judges every office damaged when it read them, so one met here was damaged
        // after.
        if is_office(state) {
            let candidates = match choice {
                Ok((head, source)) => vec![Candidate { head, source: source.to_string() }],
                Err((_, candidates, _)) => candidates,
            };
            refuse("it was damaged while heal ran".to_string(), candidates, Owner::Again);
            continue;
        }

        let (head, source) = match choice {
            Ok(chosen) => chosen,
            Err((reason, candidates, owner)) => {
                refuse(reason, candidates, choice_owner(owner, &blocking));
                continue;
            }
        };
        let candidates = vec![Candidate { head: head.clone(), source: source.to_string() }];

        if !blocking.is_empty() {
            refuse(format!(
                "it is verified against the office and checked against the redactions, and the ref of {} is damaged too: set that first",
                blocking.iter().map(|wire| format!("pallet \"{}\"", wire)).collect::<Vec<_>>().join(" and ")
            ), candidates, Owner::Cascade(blocking[0].clone()));
            continue;
        }

        let preconditions = match verify_meta_head(&state.name, &head) {
            Ok(preconditions) => preconditions,
            Err((reason, owner)) => {
                refuse(reason, candidates, owner);
                continue;
            }
        };

        if write_meta_rebuild(state, head, source, &preconditions, None, pass, report) {
            break;
        }
    }
}

/// Step 3's rebuild of damaged `@redactions` ref `state`: verified against the anchor and the
/// office as step 2 left them, and checked against the head it is rebuilt at, since its ref holds
/// none to check against. Refused while a ref in `blocking` (the office) is damaged, for the
/// reason step 1 gave when it gave one. Where step 1 verified a head (`judged`), the office was
/// checked against that head, so only that head is written, and only while the ref and its copy
/// read as step 1 judged them.
///
/// # Returns
/// * `true` - If the ref was rebuilt.
fn rebuild_redactions(state: &MetaState, blocking: &[String], judged: Option<RedactionsJudged>, tainted: &Tainted, pass: Pass, report: &mut Repair) -> bool {
    let judged = match judged {
        Some(RedactionsJudged::Refused(reason, candidates, owner)) => {
            refuse_in(report, state, pass, reason, candidates, owner);
            return false;
        }
        Some(RedactionsJudged::Verified(judged, source)) => Some((judged, source)),
        None => None,
    };

    if !blocking.is_empty() {
        let candidates = match &judged {
            Some((judged, source)) => vec![Candidate { head: judged.head.clone(), source: source.to_string() }],
            None => match choose_meta(state, tainted) {
                Ok((head, source)) => vec![Candidate { head, source: source.to_string() }],
                Err((_, candidates, _)) => candidates,
            },
        };
        refuse_in(report, state, pass, format!(
            "it is verified against the office, and the ref of {} is damaged too: set that first",
            blocking.iter().map(|wire| format!("pallet \"{}\"", wire)).collect::<Vec<_>>().join(" and ")
        ), candidates, Owner::Cascade(blocking[0].clone()));
        return false;
    }

    let (judged, source) = match judged {
        Some((judged, source)) => {
            // What the ref and its copy name now is listed, not what step 1 judged: a head judged
            // before they moved could set the ref back past them.
            if state.damage().as_ref() != Some(&judged.damage) || state.copy != judged.copy {
                let candidates = match choose_meta(state, tainted) {
                    Ok((head, source)) => vec![Candidate { head, source: source.to_string() }],
                    Err((_, candidates, _)) => candidates,
                };
                refuse_in(report, state, pass, "it changed while heal ran".to_string(), candidates, Owner::Again);
                return false;
            }
            (judged, source)
        }
        None => match choose_meta(state, tainted) {
            Ok((head, source)) => (judged_redactions(state, head), source),
            Err((reason, candidates, owner)) => {
                refuse_in(report, state, pass, reason, candidates, choice_owner(owner, blocking));
                return false;
            }
        },
    };

    let preconditions = match verify_meta_head(&state.name, &judged.head) {
        Ok(preconditions) => preconditions,
        Err((reason, owner)) => {
            refuse_in(report, state, pass, reason, vec![Candidate { head: judged.head.clone(), source: source.to_string() }], owner);
            return false;
        }
    };

    let before = report.rebuilt.len();
    write_meta_rebuild(state, judged.head.clone(), source, &preconditions, Some(&judged), pass, report);

    report.rebuilt.len() > before
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::pallet_utils::tests::{scratch, stub_parcel};

    // A user rebuild whose head the trust check refuses was never written, whatever its ref
    // reads back as; one whose write landed though it reported an error is rebuilt, with what
    // the write reported beside it.
    // Mutations: route the trust refusal through the write's error → "may have been written",
    // red; let a landed read-back drop the write's error → no note, red.
    #[test]
    fn a_user_rebuild_says_what_its_write_did() {
        let (_root, _scope) = scratch("ref-heal-user-write");
        let head = stub_parcel("head");
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Damaged, its bytes still naming the head.
        std::fs::write(&path, &head.as_bytes()[..20]).unwrap();

        TRUSTED_REFUSED.with(|refused| refused.set(true));
        READ_BACK_UNREAD.with(|unread| unread.set(true));
        let before = repair(Pass::BeforeTaint);
        let after = repair(Pass::AfterTaint);
        READ_BACK_UNREAD.with(|unread| unread.set(false));
        TRUSTED_REFUSED.with(|refused| refused.set(false));
        let before = before.unwrap();
        assert!(before.refused.is_empty() && before.rebuilt.is_empty(), "the first pass leaves it: {:?}", before.refused);
        let after = after.unwrap();
        let main = after.refused.iter().find(|refused| refused.pallet == "main").expect("main is refused");
        assert_eq!(main.reason, format!("the rebuild at {} was not written: the trust check was refused for the test", head));
        assert_eq!(std::fs::read(&path).unwrap(), &head.as_bytes()[..20], "the ref is left alone");

        let folder = path.parent().unwrap().to_path_buf();
        let rebuilt = {
            let _fault = crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&folder);
            repair(Pass::AfterTaint).unwrap()
        };
        assert!(rebuilt.refused.is_empty(), "{:?}", rebuilt.refused);
        assert_eq!(rebuilt.rebuilt.iter().map(|rebuilt| (rebuilt.pallet.as_str(), rebuilt.head.as_str())).collect::<Vec<_>>(), vec![("main", head.as_str())]);
        let said = format!("The ref of pallet \"main\" was rebuilt at {}, though its write reported: ", head);
        assert!(rebuilt.notes.iter().any(|note| note.starts_with(&said) && note.contains("injected directory-sync failure for")), "{:?}", rebuilt.notes);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().as_deref(), Some(head.as_str()));
    }

    // An unread refusal's way out is the step its read named, when that is not heal itself: it
    // reaches meta_step and the removal's refusal, and leaves the reason, so no message restates
    // its step. Mutations: keep `None` in refuse_in → the step falls back to "Settle what heal could
    // not read", red; take any frame's step → the heal-only row names "forklift heal", red.
    #[test]
    fn an_unread_refusal_names_the_step_its_read_named_other_than_heal() {
        let (_root, _scope) = scratch("ref-heal-unread-step");
        let framed = |step: &str| -> String {
            crate::error::CoreError::refusal(crate::error::RefusalCode::TrustDamaged, "the store could not give it", step).into()
        };
        let state = MetaState { name: "tags".to_string(), reference: None, copy: pallet_utils::MetaCopyState::Bytes(b"x\n".to_vec()) };
        let rows = [
            ("a concrete step", format!("parcel p does not pass verification: {}", framed("Restore the parcel from a backup")), Some("Restore the parcel from a backup")),
            ("heal first, then a concrete one", format!("{} and {}", framed("forklift heal"), framed("Restore the parcel from a backup")), Some("Restore the parcel from a backup")),
            ("only heal", format!("parcel p does not pass verification: {}", framed("forklift heal")), None),
            ("no frame", "the office could not be read".to_string(), None),
        ];
        for (row, reason, step) in rows {
            let mut report = Repair::default();
            refuse_in(&mut report, &state, Pass::AfterTaint, reason, Vec::new(), Owner::Unread(None));
            let refused = &report.refused[0];
            assert_eq!(refused.meta.as_ref().unwrap().owner, Owner::Unread(step.map(str::to_string)), "{}", row);
            assert!(!refused.reason.contains("Restore the parcel from a backup") && !refused.reason.contains("forklift heal"), "{}: {}", row, refused.reason);

            let expected = step.map_or_else(|| unread_step("@tags"), str::to_string);
            assert_eq!(meta_step(refused, &report.refused), expected, "{}", row);
            let removal = remove_meta_ref("@tags", &report.refused, &[], &mut Vec::new(), &mut None).unwrap_err();
            assert!(removal.contains("heal could not read what it needs to judge it") && removal.ends_with(&step_sentence(&expected)), "{}: {}", row, removal);
        }
    }

    // What lets a meta removal reuse a kept slot holding its bytes: no user pallet can be named
    // `@…`, and no meta name has a dot, so `@<name>.<n>` and `@<name>.copy` are that pallet's own.
    #[test]
    fn meta_kept_slots_are_no_other_pallets() {
        for name in crate::util::meta_tree::META_PALLET_NAMES {
            assert!(!name.contains('.'), "{}", name);
            assert!(pallet_utils::validate_pallet_name(&format!("@{}", name)).is_err(), "{}", name);
        }
    }

    // Mutation: judge two matching parcels as Nothing → the ref's bytes stop being a floor and
    // `--remove-ref` may drop a ref they name → red.
    #[test]
    fn bytes_matching_more_than_one_parcel_are_unknown_not_nothing() {
        let (_root, _scope) = scratch("ref-heal-judge");
        let (one, two) = (stub_parcel("1"), stub_parcel("2"));
        let clean = Tainted { hashes: BTreeSet::new(), torn: false, unread: false };

        assert_eq!(judge(vec![one.clone()], &clean), Salvage::Found(one.clone()));
        // A whole object of another kind drops out, told from its one parse.
        let blob = crate::util::gc_utils::tests::store_blob("not a parcel");
        assert_eq!(judge(vec![one.clone(), blob], &clean), Salvage::Found(one.clone()));
        assert_eq!(judge(vec![one.clone(), two.clone()], &clean), Salvage::Ambiguous(vec![one.clone(), two.clone()], false));

        let tainted = Tainted { hashes: BTreeSet::from(["f".repeat(HASH_LENGTH)]), torn: false, unread: false };
        assert_eq!(judge(vec![one.clone(), two.clone(), "f".repeat(HASH_LENGTH)], &tainted), Salvage::Ambiguous(vec![one, two], true));
    }

    // Mutation: count every salvage but Nothing → bytes naming only an unreadable object give
    // heal a head, red.
    #[test]
    fn only_bytes_naming_one_parcel_give_heal_a_head() {
        let (_root, _scope) = scratch("ref-heal-salvage-names");
        let one = stub_parcel("1");
        assert!(salvage_finds(&one.as_bytes()[..12]), "one parcel is a head");

        // A file at an object's path that reads as no kind of object: an unknown match.
        let unreadable = format!("ab{}", "c".repeat(HASH_LENGTH - 2));
        let (folder, file) = crate::util::file_utils::get_path_for_object(&unreadable).unwrap();
        let folder = std::path::PathBuf::from(folder);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(file), b"not an object").unwrap();
        let judged = salvage(&unreadable.as_bytes()[..12], &Tainted::read(&mut Vec::new()));
        assert!(matches!(judged, Ok(Salvage::Unknown(_))), "{:?}", judged);
        assert!(!salvage_finds(&unreadable.as_bytes()[..12]), "an unknown match is no head");
    }

    // A pack the store cannot list may hold what a damaged ref's bytes name, found or not, so the
    // answer is untold rather than a verdict that admits the ref's removal. Mutations: have
    // reason_for answer None → the orphan beside a loose parcel salvages Found, red;
    // count a match whose read said nothing of it as unknown → judge says Unknown, red.
    #[test]
    fn a_pack_the_store_cannot_list_leaves_what_a_refs_bytes_name_untold() {
        use crate::util::pack_utils::{pack_folder, TransportPackBuilder};

        let (_root, _scope) = scratch("ref-heal-salvage-unlisted");
        let clean = Tainted { hashes: BTreeSet::new(), torn: false, unread: false };
        let one = stub_parcel("1");
        assert_eq!(salvage(&one.as_bytes()[..12], &clean), Ok(Salvage::Found(one.clone())));

        let packed = |content: &[u8]| {
            let hash = blake3::hash(content).to_hex().to_string();
            let mut builder = TransportPackBuilder::new(&pack_folder()).unwrap();
            builder.append_full(&hash, content).unwrap();
            let artifacts = builder.finish().unwrap();
            assert_eq!(artifacts.len(), 1);
            (hash, artifacts[0].index_path.clone(), artifacts[0].data_path.clone())
        };
        let untold = |bytes: &[u8]| match salvage(bytes, &clean) {
            Ok(Salvage::Untold(why)) => why,
            other => panic!("expected untold for {:?}: {:?}", String::from_utf8_lossy(bytes), other),
        };

        // A quarantined index names the match: the gate says so, and so does the read alone.
        let (held, index, data) = packed(b"held by a pack whose data file is gone");
        crate::util::file_utils::remove_file(&data).unwrap();
        let why = untold(&held.as_bytes()[..12]);
        assert!(why.contains("cannot be looked up in full") && why.contains(index.file_name().unwrap().to_str().unwrap()), "{}", why);
        assert!(matches!(judge(vec![held.clone()], &clean), Salvage::Untold(_)), "{:?}", judge(vec![held.clone()], &clean));
        // It names no other prefix: the loose parcel is still found.
        assert_eq!(salvage(&one.as_bytes()[..12], &clean), Ok(Salvage::Found(one.clone())));
        crate::util::file_utils::remove_file(&index).unwrap();

        // An index moved aside leaves a data file that may hold any hash: no answer stands, a
        // found parcel or none.
        let (_, index, data) = packed(b"in a pack whose index was moved aside");
        crate::util::file_utils::remove_file(&index).unwrap();
        for bytes in [&one.as_bytes()[..12], one.as_bytes(), b"00000000aaaa".as_slice()] {
            let why = untold(bytes);
            assert!(why.contains(data.file_name().unwrap().to_str().unwrap()), "{}", why);
        }

        // A reader of a meta ref whose bytes salvage that way names heal, which names what it
        // could not read, never the removal heal refuses as unread. Mutation: name the removal
        // for every salvage but a found one → the step is "forklift heal --remove-ref @tags", red.
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, "tags");
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        std::fs::write(&ref_path, &one.as_bytes()[..12]).unwrap();
        let read = pallet_utils::get_meta_pallet_head("tags").unwrap_err();
        let step = crate::error::frames_in(&read).next().map(|(_, _, step)| step.to_string());
        assert_eq!(step.as_deref(), Some("forklift heal"), "{}", read);
    }

    // The removal is admitted only by a verdict a named check gave, and names it; every other
    // refusal is refused by default, and keeps the ref. One row per verdict a lost or damaged
    // @tags ref reaches with no office, and one per refusal that is no verdict. Mutations (each
    // reaches its row's `owner` assertion): read a damaged signature beside a parcel as a verdict
    // → "a damaged signature, no office" is Own, red; read a store that cannot be read as the
    // store's verdict → "an unreadable parcel" is Own, red; read a descent the store said nothing
    // of as its verdict → "the copy's history back to the ref's bytes cannot be read" is Own, red.
    #[test]
    fn a_meta_ref_is_removed_only_on_a_named_verdict_and_names_it() {
        use crate::util::file_utils::Decided;
        use crate::util::meta_tree::{stack_meta_parcel, store_blob_level, store_meta_tree};
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        let operator = |identifier: &str| crate::model::operator::Operator { identifier: identifier.to_string() };
        let parcel = |tree: String, parents: Vec<String>, by: &str, what: &str| stack_meta_parcel(tree, parents, &operator(by), what, None).unwrap();
        let tags = |what: &str| parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), "test", what);
        let object = |hash: &str| {
            let (folder, file) = crate::util::file_utils::get_path_for_object(hash).unwrap();
            std::path::PathBuf::from(folder).join(file)
        };
        let lost = b"not a head\n".to_vec();

        // Each row: its state, as the ref's bytes and the copy's (None: no copy), and what heal
        // owes the ref (None: no verdict, so the removal is refused).
        type Row = (Vec<u8>, Option<String>, Option<Verdict>);
        let rows: Vec<(&str, Box<dyn Fn() -> Row>)> = vec![
            ("nothing names a head", Box::new(|| (lost.clone(), None, Some(Verdict::NoHead)))),
            ("the copy and the ref's bytes diverge", Box::new(|| {
                let (one, two) = (tags("one"), tags("two"));
                (two.as_bytes()[..12].to_vec(), Some(one), Some(Verdict::Diverged))
            })),
            ("the copy's history back to the ref's bytes is damaged", Box::new(|| {
                let (parent, salvaged) = (tags("parent"), tags("salvaged"));
                let child = parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), vec![parent.clone()], "test", "child");
                crate::util::file_utils::remove_file(&object(&parent)).unwrap();
                std::fs::write(object(&parent), b"no object").unwrap();
                (salvaged.as_bytes()[..12].to_vec(), Some(child), Some(Verdict::Object(Decided::Damaged)))
            })),
            #[cfg(unix)]
            ("the copy's history back to the ref's bytes cannot be read", Box::new(|| {
                use std::os::unix::fs::PermissionsExt;
                let (parent, salvaged) = (tags("parent"), tags("salvaged"));
                let child = parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), vec![parent.clone()], "test", "child");
                std::fs::set_permissions(object(&parent), std::fs::Permissions::from_mode(0o000)).unwrap();
                (salvaged.as_bytes()[..12].to_vec(), Some(child), None)
            })),
            ("the ref's bytes name an object that is none", Box::new(|| {
                let unreadable = format!("ab{}", "c".repeat(HASH_LENGTH - 2));
                let path = object(&unreadable);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, b"not an object").unwrap();
                (unreadable.as_bytes()[..12].to_vec(), None, Some(Verdict::UnknownHead))
            })),
            ("a tree of its history is absent", Box::new(|| {
                let tree = store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
                let head = parcel(tree.clone(), Vec::new(), "test", "absent");
                crate::util::file_utils::remove_file(&object(&tree)).unwrap();
                (lost.clone(), Some(head), Some(Verdict::Object(Decided::Absent)))
            })),
            ("a tree of its history is damaged", Box::new(|| {
                let tree = store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
                let head = parcel(tree.clone(), Vec::new(), "test", "damaged");
                crate::util::file_utils::remove_file(&object(&tree)).unwrap();
                std::fs::write(object(&tree), b"no object").unwrap();
                (lost.clone(), Some(head), Some(Verdict::Object(Decided::Damaged)))
            })),
            ("a tree of its history is no meta tree", Box::new(|| {
                let head = parcel(store_blob_level(&[("x", b"1")]), Vec::new(), "test", "no spine");
                (lost.clone(), Some(head), Some(Verdict::NotASpine))
            })),
            ("a tree of its history is another kind's", Box::new(|| {
                let head = parcel(store_meta_tree(b"forklift-meta 1 kv", None).unwrap(), Vec::new(), "test", "kv");
                (lost.clone(), Some(head), Some(Verdict::OtherSpine))
            })),
            ("a cell of its history does not read as one", Box::new(|| {
                let cells = store_blob_level(&[("x", b"not a cell")]);
                let head = parcel(store_meta_tree(b"forklift-meta 1 tags", Some(&cells)).unwrap(), Vec::new(), "test", "malformed");
                (lost.clone(), Some(head), Some(Verdict::Malformed))
            })),
            ("a parcel of its history has two parents", Box::new(|| {
                let tree = store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
                let head = parcel(tree, vec![tags("one"), tags("two")], "test", "merge");
                (lost.clone(), Some(head), Some(Verdict::RuleBroken))
            })),
            ("a parcel of its history names no operator", Box::new(|| {
                let head = parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), "", "anonymous");
                (lost.clone(), Some(head), Some(Verdict::SignatureRefused))
            })),
            ("a damaged signature, no office", Box::new(|| {
                let head = tags("signed");
                let mut bytes = crate::util::byte_utils::number_to_vlq_bytes(1);
                bytes.push(0xff);
                crate::util::file_utils::write_file_atomically(&crate::util::sign_utils::get_signature_path(&head).unwrap(), &bytes).unwrap();
                (lost.clone(), Some(head), None)
            })),
            #[cfg(unix)]
            ("an unreadable parcel", Box::new(|| {
                use std::os::unix::fs::PermissionsExt;
                let head = tags("unreadable");
                std::fs::set_permissions(object(&head), std::fs::Permissions::from_mode(0o000)).unwrap();
                (lost.clone(), Some(head), None)
            })),
        ];

        for (row, state) in rows {
            let (_root, _scope) = scratch("ref-heal-verdicts");
            let (reference, copy, verdict) = state();
            let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
            let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
            for path in [&ref_path, &copy_path] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            std::fs::write(&ref_path, &reference).unwrap();
            if let Some(copy) = &copy {
                std::fs::write(&copy_path, format!("{}\n", copy)).unwrap();
            }

            let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
            let judged = refused.iter().find(|refused| refused.pallet == "@tags").unwrap_or_else(|| panic!("{}: @tags is refused", row));
            let owner = judged.meta.as_ref().unwrap().owner.clone();
            #[cfg(unix)]
            if let Some(copy) = &copy {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(object(copy), std::fs::Permissions::from_mode(0o644));
            }
            match verdict {
                Some(verdict) => assert_eq!(owner, Owner::Own(verdict), "{}: {}", row, judged.reason),
                None => assert!(!matches!(owner, Owner::Own(_)), "{}: {:?} ({})", row, owner, judged.reason),
            }

            let removal = remove_damaged_meta_ref("@tags", &refused, &[], &mut Vec::new());
            match verdict {
                Some(verdict) => {
                    let removed = removal.unwrap_or_else(|(error, _)| panic!("{}: {}", row, error));
                    assert_eq!(removed.kind, RemovedKind::Meta(verdict), "{}", row);
                    assert!(!ref_path.exists(), "{}: the ref is removed", row);
                }
                None => {
                    let (error, removed) = removal.unwrap_err();
                    assert!(removed.is_none() && error.contains("was not removed"), "{}: {:?} {}", row, removed, error);
                    assert_eq!(std::fs::read(&ref_path).unwrap(), reference, "{}: nothing was removed", row);
                }
            }
        }
    }

    // A ref ahead of its copy, the copy naming the parent of what the ref's own bytes name (a write
    // whose copy was not mended, a copy restored from an older backup), is linear history: it is
    // rebuilt at what its own bytes name, never refused as diverged. Mutation: check the descent
    // from the copy only → @tags is refused as its own (Diverged), and the `rebuilt` assertion
    // fails, red.
    #[test]
    fn a_meta_ref_ahead_of_its_copy_is_rebuilt_at_what_its_own_bytes_name() {
        use crate::util::meta_tree::{stack_meta_parcel, store_meta_tree};
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        let (_root, _scope) = scratch("ref-heal-ahead-of-copy");
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        let tree = || store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
        let parent = stack_meta_parcel(tree(), Vec::new(), &operator, "parent", None).unwrap();
        let child = stack_meta_parcel(tree(), vec![parent.clone()], &operator, "child", None).unwrap();
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
        let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
        for path in [&ref_path, &copy_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&ref_path, &child.as_bytes()[..12]).unwrap();
        std::fs::write(&copy_path, format!("{}\n", parent)).unwrap();

        let repair = repair_meta(Pass::AfterTaint).unwrap().repair;
        let refused: Vec<_> = repair.refused.iter().filter(|refused| refused.pallet == "@tags").collect();
        assert!(refused.is_empty(), "{:?}", refused);
        let rebuilt: Vec<_> = repair.rebuilt.iter().filter(|rebuilt| rebuilt.pallet == "@tags").map(|rebuilt| (rebuilt.head.as_str(), rebuilt.source.as_str())).collect();
        assert_eq!(rebuilt, vec![(child.as_str(), "what the ref's own bytes still name")]);
        assert_eq!(pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME).unwrap().as_deref(), Some(child.as_str()));
    }

    // A head in a format this build does not know reads whole and is no verdict on the ref: a
    // newer Forklift may have written it. Its step is the upgrade, and its removal refuses.
    // Mutation: judge a newer format as the ref's own → the step is the removal, and the removal
    // runs, red.
    #[test]
    fn a_meta_head_in_a_newer_format_names_the_upgrade_and_is_not_removed() {
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        let (_root, _scope) = scratch("ref-heal-newer-format");
        let tree = crate::util::meta_tree::store_meta_tree(b"forklift-meta 2 tags", None).unwrap();
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        let newer = crate::util::meta_tree::stack_meta_parcel(tree, Vec::new(), &operator, "from a newer forklift", None).unwrap();
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
        let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
        for path in [&ref_path, &copy_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&ref_path, b"not a head\n").unwrap();
        std::fs::write(&copy_path, format!("{}\n", newer)).unwrap();

        let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
        let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
        assert!(tags.reason.contains("forklift-meta 2 tags"), "{}", tags.reason);
        assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Newer, "{:?}", tags);
        assert_eq!(meta_step(tags, &refused), "Upgrade Forklift to a version that reads it, then run \"forklift heal\" again.");

        let mut notes = Vec::new();
        let (error, removed) = remove_damaged_meta_ref("@tags", &refused, &[], &mut notes).unwrap_err();
        assert!(removed.is_none(), "{:?}", removed);
        assert!(error.contains("was not removed") && error.contains("Upgrade Forklift to a version that reads it"), "{}", error);
        assert_eq!(std::fs::read(&ref_path).unwrap(), b"not a head\n", "nothing was removed");
    }

    // A parcel the store gives whole, its bytes matching its address, in a loose object version
    // this build does not know is no damage: a newer Forklift may have written it. Named by the
    // copy or by the ref's own bytes, its step is the upgrade, and its removal refuses.
    // Mutation: file a parse failure of verified bytes as damage (`Decided::Damaged`,
    // `Salvage::Unknown`) → the ref's own, the removal runs, red.
    #[test]
    fn a_meta_head_in_a_newer_object_version_names_the_upgrade_and_is_not_removed() {
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        for (case, by_copy) in [("the copy", true), ("the ref's own bytes", false)] {
            let (_root, _scope) = scratch("ref-heal-newer-object");
            let tree = crate::util::meta_tree::store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
            let operator = crate::model::operator::Operator { identifier: "test".to_string() };
            let parcel = crate::util::meta_tree::stack_meta_parcel(tree, Vec::new(), &operator, "tags", None).unwrap();
            let newer = crate::util::object_utils::store_copy_in_version(&parcel, 2);
            let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
            let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
            for path in [&ref_path, &copy_path] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            let (reference, copy) = match by_copy {
                true => (b"not a head\n".to_vec(), format!("{}\n", newer).into_bytes()),
                false => (newer.as_bytes()[..12].to_vec(), b"not a head\n".to_vec()),
            };
            std::fs::write(&ref_path, &reference).unwrap();
            std::fs::write(&copy_path, &copy).unwrap();

            let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
            let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
            assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Newer, "{}: {:?}", case, tags);
            assert_eq!(meta_step(tags, &refused), "Upgrade Forklift to a version that reads it, then run \"forklift heal\" again.", "{}", case);

            let mut notes = Vec::new();
            let (error, removed) = remove_damaged_meta_ref("@tags", &refused, &[], &mut notes).unwrap_err();
            assert!(removed.is_none(), "{}: {:?}", case, removed);
            assert!(error.contains("was not removed"), "{}: {}", case, error);
            assert_eq!(std::fs::read(&ref_path).unwrap(), reference, "{}: nothing was removed", case);
        }
    }

    // A rebuild heal chose but could not write is no verdict on the ref, and a lasting fault
    // meets every rerun the same way: its step is to settle why, never heal again alone, and its
    // removal refuses. Once settled, heal rebuilds it. Mutation: file a failed write as `Again`
    // → the step is "forklift heal", red; as the ref's own → the removal runs, red.
    #[cfg(unix)]
    #[test]
    fn a_meta_rebuild_heal_cannot_write_names_settling_why_and_is_not_removed() {
        use crate::util::tag_utils::TAGS_PALLET_NAME;
        use std::os::unix::fs::PermissionsExt;

        let (_root, _scope) = scratch("ref-heal-unwritten");
        let tree = crate::util::meta_tree::store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        let parcel = crate::util::meta_tree::stack_meta_parcel(tree, Vec::new(), &operator, "tags", None).unwrap();
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
        let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
        for path in [&ref_path, &copy_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&copy_path, format!("{}\n", parcel)).unwrap();
        // The ref is lost beside a copy that names its head, in a folder heal may not write.
        let folder = ref_path.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();

        let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
        assert!(tags.reason.contains(&format!("the rebuild at {} was not written", parcel)), "{:?}", tags);
        assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Unwritten(None), "{:?}", tags);
        assert_eq!(meta_step(tags, &refused), "Settle why heal could not write the ref of @tags, as its refusal says, then run \"forklift heal\" again.");

        let mut notes = Vec::new();
        let (error, removed) = remove_damaged_meta_ref("@tags", &refused, &[], &mut notes).unwrap_err();
        assert!(removed.is_none(), "{:?}", removed);
        assert!(error.contains("was not removed: heal chose its head but could not write it"), "{}", error);
        assert!(copy_path.exists(), "nothing was removed");

        // A write that failed may have landed: with its ref unread back, it is not said to be
        // unwritten, and its way out stays settling why. Mutation: judge the reason without the
        // read back → "was not written", red.
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
        READ_BACK_UNREAD.with(|unread| unread.set(true));
        let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
        READ_BACK_UNREAD.with(|unread| unread.set(false));
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
        assert!(tags.reason.contains(&format!("the rebuild at {} may have been written: the write reported", parcel)), "{:?}", tags);
        assert!(tags.reason.contains("and its ref could not be read back: the read was refused for the test"), "{:?}", tags);
        assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Unwritten(None), "{:?}", tags);

        let rebuilt = repair_meta(Pass::AfterTaint).unwrap().repair;
        assert!(rebuilt.refused.is_empty(), "{:?}", rebuilt.refused);
        assert_eq!(pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME).unwrap().as_deref(), Some(parcel.as_str()));
    }

    /// Record an unsigned redaction of a blob of `content`, and return the `@redactions` head.
    fn redact(content: &str) -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::util::redaction_utils::{record_redaction, Redaction, REDACTIONS_PALLET_NAME};

        let mut blob = LooseObjectBuilder::build_blob(&crate::model::blob::Blob { content: content.as_bytes().to_vec() });
        blob.store().unwrap();
        let redaction = Redaction { hash: blob.hash.clone(), reason: "leaked".to_string(), redacted_at: 0, chunks: None };
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        record_redaction(&redaction, &crate::util::meta_tree::MetaWriter::Unsigned, &operator).unwrap();
        pallet_utils::get_meta_pallet_head(REDACTIONS_PALLET_NAME).unwrap().unwrap()
    }

    // The office was checked against the @redactions head step 1 chose, so step 3 writes that
    // head or none. Mutation: have step 3 choose again from the ref as it then reads → the copy
    // moved back to an older head that verifies is written, red.
    #[test]
    fn redactions_is_written_at_the_head_the_office_was_checked_against_or_not_at_all() {
        use crate::util::redaction_utils::REDACTIONS_PALLET_NAME;

        let (_root, _scope) = scratch("ref-heal-redactions-pinned");
        let older = redact("one");
        let chosen = redact("two");
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, REDACTIONS_PALLET_NAME);
        std::fs::write(&ref_path, b"not a head\n").unwrap();

        let state_named = |notes: &mut Vec<String>| meta_states(notes).unwrap().into_iter()
            .find(|state| is_redactions(state)).unwrap();
        let mut notes = Vec::new();
        let judged = judged_redactions(&state_named(&mut notes), chosen.clone());

        // Between step 1 and step 3, the copy moves back to the older head.
        std::fs::write(pallet_utils::get_meta_copy_path(REDACTIONS_PALLET_NAME), format!("{}\n", older)).unwrap();
        let state = state_named(&mut notes);
        let clean = Tainted { hashes: BTreeSet::new(), torn: false, unread: false };
        let mut report = Repair::default();

        let rebuilt = rebuild_redactions(&state, &[], Some(RedactionsJudged::Verified(judged, FROM_COPY)), &clean, Pass::AfterTaint, &mut report);
        assert!(!rebuilt && report.rebuilt.is_empty(), "{:?}", report.rebuilt);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(report.refused[0].reason, "it changed while heal ran");
        assert_eq!(report.refused[0].candidates.iter().map(|c| c.head.as_str()).collect::<Vec<_>>(), vec![older.as_str()],
            "what the copy names now, not what step 1 judged");
        assert_eq!(std::fs::read(&ref_path).unwrap(), b"not a head\n", "nothing was written");
    }
}
