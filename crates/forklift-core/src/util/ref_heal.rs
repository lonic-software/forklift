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
    /// Heal has no head it can rebuild it at: nothing names one (neither its copy nor its own
    /// bytes), or what does is several parcels, one that is possibly lost, a copy and bytes that
    /// diverge, or a head whose history does not verify. Heal never removes a meta ref, so the way
    /// out is a backup ([`unrebuildable_step`]).
    Unrebuildable,

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

    /// The rebuilds whose ref (or a meta ref's copy) landed but is not durable, each said in a
    /// sentence: heal synced its folder again, and neither that sync nor a taint recording it
    /// held. A later pass reads such a ref as undamaged, so the pass that wrote it is the only one
    /// that can say so, and heal ends the run with it.
    pub undurable: Vec<String>,
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

/// The note for the ref of `pallet` at `path`, which checkout `which` stands on, when something
/// that is not a file stands there. A folder at a user ref's path is where the refs of the
/// pallets named under it are kept, so moving it aside would take theirs: the way out is to move
/// the checkout off the pallet instead.
fn checkout_not_a_file_note(which: &str, pallet: &str, path: &Path) -> String {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => format!(
            "A folder stands at the ref of pallet \"{}\" (\"{}\"), which {} stands on, so heal did not judge it. \
            It may hold the refs of pallets named under \"{}/\", so do not move it aside: \
            run \"forklift shift <pallet>\" in {} to put it on another pallet, then run heal again.",
            pallet, path.to_string_lossy(), which, pallet, which
        ),
        _ => not_a_file_note(pallet, path),
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
            // Judged as it stands, as above: a file at the ref's path was judged there, anything
            // else standing there (a folder, a link) is no ref and is named, and a path whose state
            // cannot be read tells nothing of a loss.
            Ok(position) if position.base.is_some() => {
                let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, &position.pallet);
                match pallet_utils::read_file_state(&path) {
                    Ok(pallet_utils::MetaCopyState::Bytes(_)) => {}
                    Ok(pallet_utils::MetaCopyState::Absent) => {
                        found.entry(position.pallet).or_insert(RefDamage::Missing);
                    }
                    Ok(pallet_utils::MetaCopyState::Other) => push_note(notes, checkout_not_a_file_note(&which, &position.pallet, &path)),
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

/// Whether the store has decided that `head` is gone, and the durability taint does not account
/// for it. Read as [`judge`] reads a candidate: a refill, a redaction, a withholding or a pack the
/// store cannot list leaves an absence undecided, which is not this. Bytes held whole, held
/// damaged, of another kind or of a newer build are not absent, and a head the taint covers is not
/// read at all, since the taint recovery may yet restore it.
fn head_decided_absent(head: &str, tainted: &Tainted) -> bool {
    use crate::util::file_utils::{Decided, ReadFault};

    !tainted.covers(head) && matches!(object_utils::load_parcel_judged(head), Err((_, ReadFault::Decided(Decided::Absent))))
}

/// Whether the store gives `head` as a whole parcel, and the durability taint does not cover it.
fn head_held(head: &str, tainted: &Tainted) -> bool {
    !tainted.covers(head) && matches!(object_utils::load_parcel_judged(head), Ok(Ok(_)))
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
                    Err(error) => {
                        let mut read = read_back(PalletNamespace::User, &damaged.pallet, &head);
                        let attempt = WriteAttempt::of_ref(pass, PalletNamespace::User, &damaged.pallet, &head, Some(&error), &read);
                        let attempted = attempt.is_some();
                        if let Some((_, settled)) = attempt.and_then(|attempt| settle_attempts(vec![attempt], &mut report).pop()) {
                            read = settled;
                        }

                        match read {
                            ReadBack::Landed => landed(&mut report, PalletNamespace::User, &damaged.pallet, head, source, Some(&error)),
                            // The pass after the taint recovery may read a write that landed as an
                            // undamaged ref, and then says nothing of it: the head is named here.
                            ReadBack::Unread(why) if attempted => push_note(&mut report.notes, format!(
                                "The ref of pallet \"{}\" may have been rebuilt at {}: the write reported {}, and its ref could not be read back: {}.",
                                damaged.pallet, head, error, why.trim_end_matches('.')
                            )),
                            // The first pass leaves a failed write to the second, which reports it.
                            _ if pass == Pass::BeforeTaint => {}
                            read => report.refused.push(Refused {
                                pallet: damaged.pallet.clone(),
                                reason: unwritten_reason(&head, &error, &read),
                                candidates: listed(&evidence.candidates),
                                meta: None,
                            }),
                        }
                    }
                }
            }
        }
    }

    Ok(report)
}

/// Whether a ref write's `error` says a folder was not synced and no taint records that, so what
/// it renamed into place is visible but not durable, and nothing else heals it: heal syncs it
/// again itself ([`mended`]) before it reports the rebuild.
fn taint_unrecorded(error: &RefUpdateError) -> bool {
    matches!(error, RefUpdateError::Failed(error) if crate::util::file_utils::is_taint_unrecorded(error))
}

/// Mend a rebuild of the ref of pallet `name` in `namespace` that landed although its write
/// reported `error`: one that says a folder was not synced and no taint records it is synced again
/// ([`sync_again`]); any other already stands durable or has a taint recording it.
fn mended(error: &RefUpdateError, namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
    match taint_unrecorded(error) {
        true => sync_again(namespace, name),
        false => Ok(None),
    }
}

/// Sync again the folder of each file a rebuild of the ref of pallet `name` in `namespace` wrote:
/// its ref, and a meta ref's copy. A sync that fails records a taint, as any write's does.
///
/// # Returns
/// * `Ok(None)` - If every folder synced.
/// * `Ok(Some(error))` - If a sync failed and a taint records it, which the taint recovery mends.
/// * `Err(why)` - If a sync failed and no taint records it either: which file is not durable, and
///   the error.
fn sync_again(namespace: PalletNamespace, name: &str) -> Result<Option<String>, String> {
    let mut written = vec![("ref", pallet_utils::get_pallet_ref_path(namespace, name))];
    if namespace == PalletNamespace::Meta {
        written.push(("copy", pallet_utils::get_meta_copy_path(name)));
    }

    let mut tainted = None;
    // A copy the write did not get as far as leaves nothing to sync.
    for (file, path) in written.iter().filter(|(_, path)| path.symlink_metadata().is_ok()) {
        let Some(folder) = path.parent() else { continue };
        match file_utils::sync_dir_or_taint(folder, &[path]) {
            Ok(()) => {}
            Err(error) if file_utils::is_taint_unrecorded(&error) => return Err(format!("its {} is not durable: {}", file, error)),
            Err(error) => { tainted.get_or_insert(error); }
        }
    }
    Ok(tainted)
}

/// The note for a rebuild of the ref of `pallet` (as it is written on the wire) whose folder heal
/// synced again after its write left it unsynced, where that sync failed too and a taint records
/// it: `error`.
fn synced_again(pallet: &str, error: &str) -> String {
    format!("Heal synced the folder of the ref of pallet \"{}\" again, which reported: {}", pallet, error)
}

/// The sentence for a rebuild of the ref of `pallet` (as it is written on the wire) at `head` that
/// landed and is not durable, `why` naming which file and the error, though heal synced it again.
fn undurable(pallet: &str, head: &str, why: &str) -> String {
    format!(
        "The ref of pallet \"{}\" was rebuilt at {}, but {}. Heal synced its folder again, and neither \
        that sync nor a taint recording it held, so a crash can still lose the rebuild; a later heal reads \
        the ref as undamaged and does not sync it again.",
        pallet, head, why.trim_end_matches('.')
    )
}

/// The note for a rebuild of the ref of `pallet` (as it is written on the wire) at `head` that
/// landed although its write reported `error`: a failure after the ref's rename (its folder not
/// synced, a taint standing) leaves the ref written, and what the write reported stands beside it.
fn landed_despite(pallet: &str, head: &str, error: &RefUpdateError) -> String {
    format!("The ref of pallet \"{}\" was rebuilt at {}, though its write reported: {}", pallet, head, error)
}

/// The sentence for a rebuild of the ref of `pallet` (as it is written on the wire) at `head`
/// whose ref could not be read back, and which is not durable if it landed, `why` naming which
/// file and the error, though heal synced it again.
fn undurable_if_landed(pallet: &str, head: &str, why: &str) -> String {
    format!(
        "The ref of pallet \"{}\" may have been rebuilt at {}, and if it was, {}. Heal synced its folder again, and \
        neither that sync nor a taint recording it held, so a crash can still lose the rebuild; a later heal reads \
        the ref as undamaged and does not sync it again.",
        pallet, head, why.trim_end_matches('.')
    )
}

/// Report the rebuild of the ref of pallet `name` in `namespace` at `head`, found at `source`,
/// whose ref read back at `head` though its write may have reported `error`: rebuilt, with what
/// the write reported beside it once what it left unsynced with no taint recording it is synced
/// again ([`mended`]), or not durable when that sync does not hold either.
fn landed(report: &mut Repair, namespace: PalletNamespace, name: &str, head: String, source: String, error: Option<&RefUpdateError>) {
    let wire = pallet_utils::PalletRef { namespace, name: name.to_string() }.to_wire();
    let Some(error) = error else {
        report.rebuilt.push(Rebuilt { pallet: wire, head, source });
        return;
    };
    match mended(error, namespace, name) {
        Ok(again) => {
            push_note(&mut report.notes, landed_despite(&wire, &head, error));
            if let Some(again) = again {
                push_note(&mut report.notes, synced_again(&wire, &again));
            }
            report.rebuilt.push(Rebuilt { pallet: wire, head, source });
        }
        Err(why) => report.undurable.push(undurable(&wire, &head, &why)),
    }
}

/// A ref write [`Pass::BeforeTaint`] made that may have landed, though its ref could not be read
/// back. The pass after the taint recovery would read a ref it landed in as undamaged and judge
/// nothing, so the pass that wrote it settles it ([`settle_attempts`]).
///
/// Where it is taken apart, an irrefutable `let` does it, so a further variant fails to build
/// there until that site decides it.
#[derive(Debug)]
enum WriteAttempt {
    /// The write of the ref of pallet `name` in `namespace` at `head`, and what it reported
    /// (`None`: it returned `Ok`).
    Ref { namespace: PalletNamespace, name: String, head: String, error: Option<RefUpdateError> },
}

impl WriteAttempt {
    /// The attempt a write of the ref of pallet `name` in `namespace` at `head` is in `pass`, its
    /// write having reported `error` (`None`: `Ok`) and its ref read back as `read`. Only a write
    /// that returned `Ok` or failed with [`RefUpdateError::Failed`] can have landed, and one that
    /// stopped on damaged tombstones is the pass's stop instead; only an unread ref leaves it
    /// unanswered; and [`Pass::AfterTaint`] refuses what it cannot answer, with no pass after it.
    fn of_ref(pass: Pass, namespace: PalletNamespace, name: &str, head: &str, error: Option<&RefUpdateError>, read: &ReadBack) -> Option<WriteAttempt> {
        let may_have_landed = match error {
            None => true,
            Some(RefUpdateError::Failed(error)) => !crate::util::tombstone_utils::is_damage(error),
            Some(_) => false,
        };
        (pass == Pass::BeforeTaint && matches!(read, ReadBack::Unread(_)) && may_have_landed).then(|| WriteAttempt::Ref {
            namespace, name: name.to_string(), head: head.to_string(), error: error.cloned(),
        })
    }
}

/// Settle `attempts`, each at its write: read its ref back once more, and for a write that failed,
/// sync again what it left unsynced with no taint recording that ([`mended`]) while the ref still
/// cannot be read, since that sync needs only the files the write left, not its landing. A sync
/// that does not hold is said in `report.undurable`. A read that finds the head leaves that sync to
/// the write's own judgment ([`landed`]), which reports the rebuild.
///
/// # Returns
/// Each attempt, with what its ref read back as now, which the write is judged on in place of its
/// first read. Nothing is refused or deferred here.
fn settle_attempts(attempts: Vec<WriteAttempt>, report: &mut Repair) -> Vec<(WriteAttempt, ReadBack)> {
    attempts.into_iter().map(|attempt| {
        let WriteAttempt::Ref { namespace, name, head, error } = &attempt;
        let read = read_back(*namespace, name, head);
        if let (ReadBack::Unread(_), Some(error)) = (&read, error) {
            let wire = pallet_utils::PalletRef { namespace: *namespace, name: name.clone() }.to_wire();
            match mended(error, *namespace, name) {
                Ok(None) => {}
                Ok(Some(again)) => push_note(&mut report.notes, synced_again(&wire, &again)),
                Err(why) => report.undurable.push(undurable_if_landed(&wire, head, &why)),
            }
        }
        (attempt, read)
    }).collect()
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

    let kept = kept_path(pallet);

    pallet_utils::remove_damaged_ref(pallet, bytes, &kept).map_err(|error| error.to_string())?;

    Ok(Removed { pallet: pallet.to_string(), kept: kept.to_string_lossy().into_owned() })
}

/// The first free path under the removed-refs folder for bytes removed from `pallet`'s ref. A
/// pallet's numbered slot can be another pallet's own (`a.1` is pallet `a`'s second and pallet
/// `a.1`'s first), so only a free one is taken.
fn kept_path(pallet: &str) -> std::path::PathBuf {
    let folder = forklift_root().join(FOLDER_NAME_REMOVED_REFS);
    let stem = pallet.replace('/', "%2F");
    (0..).map(|n| folder.join(match n { 0 => stem.clone(), n => format!("{}.{}", stem, n) }))
        // A slot whose state cannot be read is taken as free, and keeping the bytes there then
        // refuses it ([`pallet_utils`]'s keep reads it back), so it is never written over.
        .find(|path| std::fs::symlink_metadata(path).is_err())
        .expect("an unbounded range always yields a free name")
}

/// The step that lets a later heal judge meta ref `refused` again, among the refusals `all` of
/// the same pass: restoring it from a backup, where heal has no head it can rebuild it at (or
/// replacing what stands at its copy's path, while that blocks the copy); for one
/// that waits on another meta ref's
/// refusal, that ref's step; for one beside an unborn office, what brings the office back; for one
/// over what a newer Forklift may have written, the upgrade; for one heal could not read, settling
/// what it names; and otherwise heal again. A command, or sentences
/// when the way out is by hand.
pub fn meta_step(refused: &Refused, all: &[Refused]) -> String {
    let mut at = refused;
    // A cascade names the office or @redactions, whose own refusals do not cascade back.
    for _ in 0..3 {
        match at.meta.as_ref().map(|meta| &meta.owner) {
            Some(Owner::Cascade(cause)) => match all.iter().find(|other| other.pallet == *cause) {
                Some(cause) => at = cause,
                None => break,
            },
            // While something not a file stands at the copy's path, replacing it is the way out
            // a read names too.
            Some(Owner::Unrebuildable) => return match at.meta.as_ref().is_some_and(|meta| pallet_utils::copy_blocks(&at.pallet[1..], &meta.copy)) {
                true => pallet_utils::blocked_copy_step(&at.pallet[1..], "run \"forklift heal\" again"),
                false => unrebuildable_step(&at.pallet),
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

/// The way out of meta ref `wire`'s refusal where heal has no head it can rebuild it at: heal
/// never removes a meta ref, so the ref and its copy are restored from a backup.
pub fn unrebuildable_step(wire: &str) -> String {
    format!("Restore the ref of {} and its copy from a backup, then run \"forklift heal\" again.", wire)
}

/// `step`, a command or sentences from [`meta_step`], as a sentence of its own.
pub fn step_sentence(step: &str) -> String {
    match step.starts_with("forklift ") {
        true => format!("Run \"{}\".", step),
        false => step.to_string(),
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

    /// Whether the ref's bytes name a head the store has decided is absent, beside a copy naming
    /// another head the store holds; never for the office. Judged once, when the state is read, so
    /// every reader of [`MetaState::damage`] sees the same answer.
    stale: bool,
}

impl MetaState {
    fn wire(&self) -> String {
        meta_wire(&self.name)
    }

    /// How the ref is damaged: lost beside a copy, holding bytes that are not a head, or naming a
    /// head the store decided is gone beside a copy whose head it holds. `None` for an intact ref,
    /// and for a pallet with neither a ref nor a copy, which is unborn.
    fn damage(&self) -> Option<RefDamage> {
        match &self.reference {
            Some(bytes) if self.stale => Some(RefDamage::AbsentHead(bytes.clone())),
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
            Ok(copy) => states.push(MetaState { name, reference, copy, stale: false }),
            Err(error) => skipped.push((name.clone(), format!(
                "The copy of pallet \"{}\"'s ref could not be read, so heal did not judge that ref: {}", meta_wire(&name), error
            ))),
        }
    }

    // Only a ref and a copy naming two heads can be stale, so a warehouse whose copies agree reads
    // neither the taint nor the store here. The office is never judged stale: what heal does with
    // an office whose head is gone beside a stored copy is not decided, so it is left to the copy
    // mend's refusal, which keeps both files. A taint that cannot be read covers every head, so
    // nothing is judged stale beside it; its note is for the passes that salvage, which read it
    // themselves, and would say here that salvaged objects count as lost where none was salvaged.
    let mut tainted: Option<Tainted> = None;
    for state in &mut states {
        if is_office(state) {
            continue;
        }
        let (Some(head), Some(copy)) = (state.reference.as_deref().and_then(pallet_utils::parse_ref_bytes), state.copy_head()) else {
            continue;
        };
        if head == copy {
            continue;
        }
        let tainted = tainted.get_or_insert_with(|| Tainted::read(&mut Vec::new()));
        state.stale = head_decided_absent(&head, tainted) && head_held(&copy, tainted);
    }

    Ok(states)
}

/// Which of a meta ref's two heads descends from the other along first parents.
#[derive(Debug, PartialEq, Eq)]
enum Descent {
    /// The copy's head descends from what the ref's bytes name.
    CopyAhead,
    /// What the ref's bytes name descends from the copy's head.
    RefAhead,
    /// Neither descends from the other.
    Neither,
}

/// One first-parent walk from a head toward the head it may descend from.
struct DescentWalk {
    at: Option<String>,
    target: String,
    seen: BTreeSet<String>,
    failed: Option<(String, crate::util::file_utils::ReadFault)>,
    reached: bool,
}

impl DescentWalk {
    fn new(head: &str, target: &str) -> Self {
        DescentWalk { at: Some(head.to_string()), target: target.to_string(), seen: BTreeSet::new(), failed: None, reached: false }
    }

    /// Take one step; false once the walk has reached its target, ended or failed.
    fn step(&mut self) -> bool {
        let Some(at) = self.at.take() else { return false };
        if at == self.target {
            self.reached = true;
            return false;
        }
        if !self.seen.insert(at.clone()) {
            return false;
        }
        match object_utils::load_parcel_faulted(&at) {
            Ok(parcel) => self.at = parcel.parents.first().cloned(),
            Err(error) => self.failed = Some(error),
        }
        self.at.is_some()
    }
}

/// Which of `copy` and `salvaged` (two different heads) descends from the other along first
/// parents. The two walks step in turn, so the one that descends is found in as many steps as
/// it lies ahead, however long the other's history; a read that failed on one walk decides
/// nothing while the other can still settle it.
///
/// # Returns
/// * `Err((copy_side, error))` - Neither walk settled it and one failed: whether it was the
///                               copy's walk, and what the failed parcel read tells.
fn which_descends(copy: &str, salvaged: &str) -> Result<Descent, (bool, (String, crate::util::file_utils::ReadFault))> {
    let mut from_copy = DescentWalk::new(copy, salvaged);
    let mut from_ref = DescentWalk::new(salvaged, copy);

    loop {
        let copy_going = from_copy.step();
        if from_copy.reached {
            return Ok(Descent::CopyAhead);
        }
        let ref_going = from_ref.step();
        if from_ref.reached {
            return Ok(Descent::RefAhead);
        }
        if !copy_going && !ref_going {
            break;
        }
    }

    match (from_copy.failed, from_ref.failed) {
        (Some(error), _) => Err((true, error)),
        (None, Some(error)) => Err((false, error)),
        (None, None) => Ok(Descent::Neither),
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
/// was not mended, a copy restored from an older backup) is rebuilt at what its own bytes name,
/// while the store holds that head. A ref whose head the store decided is gone beside a copy whose
/// head it holds ([`RefDamage::AbsentHead`]) salvages nothing, so it is rebuilt at the copy's head,
/// even where the copy is the older of the two: the ref goes back to the last head that can be
/// read, and the note [`write_meta_rebuild`] leaves names the head it named.
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

    // Whose a salvage that names no head heal takes is: nothing named with no copy, several
    // parcels, or a match the store decided is unknown leave heal no head to rebuild at; a newer
    // build's for a match this build cannot parse, unread where the store said nothing (a match or
    // a pack it could not read).
    match salvaged {
        Salvage::Nothing => match copy {
            Some(head) => Ok((head, FROM_COPY)),
            None => Err(("neither its copy nor its own bytes name a head".to_string(), candidates, Owner::Unrebuildable)),
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
                    // History the store decided is damaged or gone proves neither the newer, and
                    // both still name a head; a read that said nothing of it is no verdict.
                    let untold = |(descendant, ancestor): (&str, &str), (error, fault): (String, crate::util::file_utils::ReadFault), candidates| (format!(
                        "whether {} descends from {} could not be told: {}",
                        descendant, ancestor, error.trim_end_matches('.')
                    ), candidates, match fault {
                        crate::util::file_utils::ReadFault::Decided(_) => Owner::Unrebuildable,
                        crate::util::file_utils::ReadFault::Newer => Owner::Newer,
                        crate::util::file_utils::ReadFault::Untold => Owner::Unread(None),
                    });

                    match which_descends(&copy, &salvaged) {
                        Ok(Descent::CopyAhead) => Ok((copy, FROM_COPY)),
                        Ok(Descent::RefAhead) => Ok((salvaged, "what the ref's own bytes still name")),
                        Ok(Descent::Neither) => Err((format!(
                            "its copy names {} and its own bytes name {}, and neither descends along first parents from the other, so nothing proves either the newer",
                            copy, salvaged
                        ), candidates, Owner::Unrebuildable)),
                        Err((false, error)) => Err(untold((&format!("what its own bytes name, {},", salvaged), &format!("its copy's head {}", copy)), error, candidates)),
                        Err((true, error)) => Err(untold((&format!("its copy's head {}", copy), &format!("{}, what its own bytes name,", salvaged)), error, candidates)),
                    }
                }
            }
        }
        Salvage::Ambiguous(parcels, _) => {
            candidates.extend(parcels.into_iter().map(|head| Candidate { head, source: Source::Salvage.describe() }));
            Err(("its own bytes name one of several parcels, not which".to_string(), candidates, Owner::Unrebuildable))
        }
        Salvage::Unknown(why) => Err((why, candidates, Owner::Unrebuildable)),
        Salvage::Untold(why) => Err((why, candidates, Owner::Unread(None))),
        Salvage::Newer(why) => Err((why, candidates, Owner::Newer)),
    }
}

/// Verify `head` as the whole history of meta pallet `name`, against the trust anchor and the
/// office as they are now, and return the preconditions that pin both for the write.
///
/// # Returns
/// * `Err((reason, owner))` - Why not, and whose refusal that is: [`Owner::Unrebuildable`] when
///                            `head` fails verification, [`Owner::OfficeUnborn`] when the office is unborn
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
/// reason `owner` says: no head it can rebuild at ([`Owner::Unrebuildable`]) is still the office's
/// while the office is refused (`blocking`) or unborn beside the trust file's anchor, since heal
/// then judged it against no office, and once the office stands a later heal may rebuild it.
fn choice_owner(owner: Owner, blocking: &[String]) -> Owner {
    if owner != Owner::Unrebuildable {
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
/// * `Err((reason, owner))` - [`Owner::Unrebuildable`] when the verdict is on `head`'s history (a
///                            rule it breaks, or a parcel of it the store decided is absent or
///                            holds damaged); [`Owner::Newer`] when what it needs is in a format this
///                            build does not know; [`Owner::OfficeUnborn`] or [`Owner::Trust`]
///                            when the trust state it is checked against refuses, read whole;
///                            [`Owner::Unread`] when the refusal says nothing of it.
fn verify_meta_against(name: &str, head: &str, anchor: Option<&crate::util::office_utils::TrustAnchor>, office_head: Option<&str>) -> Result<(), (String, Owner)> {
    use crate::util::meta_tree::{MetaCheckError, TrustGap};

    let Some(rules) = crate::util::meta_tree::meta_rules(name) else {
        return Err(("it is not a meta pallet heal has rules to verify against".to_string(), Owner::Unrebuildable));
    };

    crate::util::meta_tree::verify_meta_update(name, rules, crate::util::meta_tree::Admission::Audit, anchor, office_head, None, head)
        .map(|_| ())
        .map_err(|error| {
            let owner = match error {
                MetaCheckError::Verdict(_) => Owner::Unrebuildable,
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

/// Set an intact meta ref's copy to the ref's bytes, when the copy is lost or differs and the
/// store gives the ref's head whole.
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

    // The copy is set only toward a head the store gives whole: over any other answer, it may be
    // the one file still naming a head a reader can load. The store's outer error is only a
    // decided answer or an untold one, never bytes of a newer build, which come back inside `Ok`.
    // Before the taint the refusal is silent, as the error arm below is: the pass after the taint
    // recovery reads the store again and mends a head the recovery made readable, or says why
    // not. A run that returns before that pass reports nothing of this copy; the next run does.
    let refused = match object_utils::load_parcel_judged(head) {
        Ok(Ok(_)) | Ok(Err(object_utils::NotAParcel::Newer(_))) => None,
        Ok(Err(object_utils::NotAParcel::Other(_))) => Some("is not a parcel".to_string()),
        Err((_, crate::util::file_utils::ReadFault::Decided(crate::util::file_utils::Decided::Absent))) => Some("is not in the store".to_string()),
        Err((error, crate::util::file_utils::ReadFault::Decided(_))) => Some(format!("cannot be read: {}", error)),
        Err((error, _)) => Some(format!("could not be read from the store: {}", error)),
    };

    if let Some(said) = refused {
        if pass == Pass::AfterTaint {
            push_note(notes, match state.copy_head() {
                Some(other) => format!(
                    "The copy of pallet \"{}\"'s ref names {}, and was left as it is: the ref's head {} {}, so the copy was not set to it.",
                    state.wire(), other, head, said
                ),
                None => format!(
                    "The copy of pallet \"{}\"'s ref is lost or damaged, and was left as it is: the ref's head {} {}, so the copy was not set to it.",
                    state.wire(), head, said
                ),
            });
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
///    rebuilt at. Then every meta ref that reads has a lost or differing copy set to it, where
///    the store gives its head whole, and every other lost or damaged one, a ref naming a head
///    the store decided is gone beside a copy whose head it holds among them, is rebuilt the
///    same way, checked against the redactions
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
            // The run ends on the trust refusal, and what landed not durable is said beside it.
            notes.append(&mut report.undurable);
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
                crate::util::office_utils::AnchorRefusal::Refused => Owner::Unrebuildable,
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
    /// How many of the next [`read_back`] calls answer [`ReadBack::Unread`], since no fixture
    /// fails the read of a ref heal could judge right after its write failed.
    static READ_BACK_UNREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };

    /// Run before the next [`read_back`] that reads the ref, once.
    static BEFORE_READ_BACK: std::cell::Cell<Option<Box<dyn FnOnce()>>> = const { std::cell::Cell::new(None) };
}

/// Read back the ref of pallet `name` after heal wrote it at `head`.
fn read_back(namespace: PalletNamespace, name: &str, head: &str) -> ReadBack {
    #[cfg(test)]
    if READ_BACK_UNREAD.with(|unread| unread.replace(unread.get().saturating_sub(1)) > 0) {
        return ReadBack::Unread("the read was refused for the test".to_string());
    }
    #[cfg(test)]
    if let Some(before) = BEFORE_READ_BACK.with(std::cell::Cell::take) {
        before();
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
    // A ref rebuilt over a head the store decided is gone loses the only record of that head. So
    // the head it named is noted before the write, in the past tense: the state heal judged and
    // what it set out to do. That holds whichever way the write then goes. What this pass learned
    // of the write is reported beside it; for a write it could not read back, that is the attempt,
    // not where the ref ended.
    let gone = match &damage {
        RefDamage::AbsentHead(bytes) => pallet_utils::parse_ref_bytes(bytes),
        _ => None,
    };
    if let Some(gone) = gone {
        push_note(&mut report.notes, format!(
            "The ref of pallet \"{}\" named {}, which was not in the store, when heal set out to rebuild it at {} from {}.",
            state.wire(), gone, head, source
        ));
    }
    let written = pallet_utils::repair_meta_pallet_head(&state.name, &damage, &state.copy, &head, preconditions, judged);

    let mut read = read_back(PalletNamespace::Meta, &state.name, &head);
    let attempt = WriteAttempt::of_ref(pass, PalletNamespace::Meta, &state.name, &head, written.as_ref().err(), &read);
    if let Some((_, settled)) = attempt.and_then(|attempt| settle_attempts(vec![attempt], report).pop()) {
        read = settled;
    }

    let (reason, owner) = match written {
        // As in the user pass: the refusal is the answer, and what this pass rebuilt stays
        // reported.
        Err(RefUpdateError::Failed(error)) if crate::util::tombstone_utils::is_damage(&error) => {
            report.stopped = Some(error);
            return true;
        }
        // As in the user pass.
        _ if matches!(read, ReadBack::Landed) => {
            landed(report, PalletNamespace::Meta, &state.name, head, source.to_string(), written.as_ref().err());
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

    // The pass after the taint recovery keeps a deferral only for a ref it leaves unjudged, so a
    // deferral is not where the outcome of a write this pass attempted can rest: it is noted too.
    if pass == Pass::BeforeTaint {
        push_note(&mut report.notes, format!(
            "The pass before the taint recovery left the ref of pallet \"{}\" for the pass after it: {}.",
            state.wire(), crate::error::frames_without_steps(&reason).trim_end_matches('.')
        ));
    }
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
    // A rebuild that landed with its folder not synced and no taint recording that is neither.
    // Mutations: route the trust refusal through the write's error → "may have been written",
    // red; let a landed read-back drop the write's error → no note, red; let a landed read-back
    // win over an unrecorded taint → main is rebuilt, red.
    // Unix only: a folder sync is a no-op elsewhere, so no injected sync failure can fire.
    #[cfg(unix)]
    #[test]
    fn a_user_rebuild_says_what_its_write_did() {
        let (_root, _scope) = scratch("ref-heal-user-write");
        let head = stub_parcel("head");
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Damaged, its bytes still naming the head.
        std::fs::write(&path, &head.as_bytes()[..20]).unwrap();

        TRUSTED_REFUSED.with(|refused| refused.set(true));
        READ_BACK_UNREAD.with(|unread| unread.set(usize::MAX));
        let before = repair(Pass::BeforeTaint);
        let after = repair(Pass::AfterTaint);
        READ_BACK_UNREAD.with(|unread| unread.set(0));
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

        // Landed, its folder not synced and no taint recording that: the pass that wrote it, the
        // first as heal runs them, syncs the folder again, and the rebuild stands once that holds.
        // Mutation: skip the sync again (`sync_again` answers `Ok(None)` at once) → one sync, red.
        std::fs::write(&path, &head.as_bytes()[..20]).unwrap();
        let (mended, syncs) = {
            let _fault = crate::util::file_utils::SyncDirFaultGuard::failing_exactly_once(&folder);
            let _taint = crate::util::file_utils::TaintRecordFaultGuard::failing();
            let mended = repair(Pass::BeforeTaint).unwrap();
            (mended, crate::util::file_utils::sync_dir_attempts().into_iter().filter(|synced| *synced == folder).count())
        };
        assert_eq!(syncs, 2, "the write's sync, then heal's own");
        assert!(mended.undurable.is_empty(), "{:?}", mended.undurable);
        assert_eq!(mended.rebuilt.iter().map(|rebuilt| (rebuilt.pallet.as_str(), rebuilt.head.as_str())).collect::<Vec<_>>(), vec![("main", head.as_str())]);
        assert!(mended.notes.iter().any(|note| note.starts_with(&said) && note.contains("injected taint-record failure")), "{:?}", mended.notes);

        // Neither that sync nor a taint recording it holds: the first pass says so, since the
        // second reads the ref as undamaged and says nothing. Mutation: drop the sentence, as the
        // first pass drops a failed write → nothing said, red.
        std::fs::write(&path, &head.as_bytes()[..20]).unwrap();
        let (before, after) = {
            let _fault = crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&folder);
            let _taint = crate::util::file_utils::TaintRecordFaultGuard::failing();
            (repair(Pass::BeforeTaint).unwrap(), repair(Pass::AfterTaint).unwrap())
        };
        assert!(before.rebuilt.is_empty() && before.refused.is_empty(), "{:?}", before);
        assert_eq!(before.undurable, vec![format!(
            "The ref of pallet \"main\" was rebuilt at {}, but its ref is not durable: injected directory-sync failure for \"{}\" \
            (additionally, while recording a durability taint for the affected object(s): injected taint-record failure). Heal synced \
            its folder again, and neither that sync nor a taint recording it held, so a crash can still lose the rebuild; a later \
            heal reads the ref as undamaged and does not sync it again.",
            head, folder.to_string_lossy()
        )]);
        assert!(after.rebuilt.is_empty() && after.refused.is_empty() && after.undurable.is_empty(), "{:?}", after);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().as_deref(), Some(head.as_str()));
    }

    // An unread refusal's way out is the step its read named, when that is not heal itself: it
    // reaches meta_step, and leaves the reason, so no message restates
    // its step. Mutations: keep `None` in refuse_in → the step falls back to "Settle what heal could
    // not read", red; take any frame's step → the heal-only row names "forklift heal", red.
    #[test]
    fn an_unread_refusal_names_the_step_its_read_named_other_than_heal() {
        let (_root, _scope) = scratch("ref-heal-unread-step");
        let framed = |step: &str| -> String {
            crate::error::CoreError::refusal(crate::error::RefusalCode::TrustDamaged, "the store could not give it", step).into()
        };
        let state = MetaState { name: "tags".to_string(), reference: None, copy: pallet_utils::MetaCopyState::Bytes(b"x\n".to_vec()), stale: false };
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
        }
    }

    // Mutation: drop the taint check from the predicate → a covered head reads as decided
    // absent, red. The clean half shows the fixture reaches the decided answer.
    #[test]
    fn a_head_the_taint_covers_is_never_decided_absent() {
        let (_root, _scope) = scratch("ref-heal-absent-tainted");
        let absent = blake3::hash(b"never stored here").to_hex().to_string();
        let clean = Tainted { hashes: BTreeSet::new(), torn: false, unread: false };
        let covering = Tainted { hashes: BTreeSet::from([absent.clone()]), torn: false, unread: false };

        assert!(head_decided_absent(&absent, &clean), "the store decided it is absent");
        assert!(!head_decided_absent(&absent, &covering), "the taint may yet restore it");
    }

    // A meta ref whose head the store cannot tell of, beside a copy naming a held head, is no
    // stale ref, and its copy is not set to that head. Mutations: let the predicate take an untold
    // answer → the ref reads as damaged, red; drop the copy mend's refusal → the copy takes the
    // ref's head, red.
    #[test]
    fn a_meta_ref_whose_head_the_store_cannot_tell_of_is_not_stale_and_keeps_its_copy() {
        let (_root, _scope) = scratch("ref-heal-untold");
        let held = stub_parcel("held");
        let absent = blake3::hash(b"never stored here").to_hex().to_string();
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, "tags");
        let copy_path = pallet_utils::get_meta_copy_path("tags");
        for path in [&ref_path, &copy_path] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(&ref_path, format!("{}\n", absent)).unwrap();
        std::fs::write(&copy_path, format!("{}\n", held)).unwrap();

        // With the store answering, the same files are a stale ref: the fixture reaches the verdict.
        let tags = |notes: &mut Vec<String>| meta_states(notes).unwrap().into_iter().find(|state| state.name == "tags").unwrap();
        assert_eq!(tags(&mut Vec::new()).damage(), Some(RefDamage::AbsentHead(format!("{}\n", absent).into_bytes())));

        let _failing = crate::util::pack_utils::fail_unindexed_holders("the pack registry could not be read");
        let clean = Tainted { hashes: BTreeSet::new(), torn: false, unread: false };
        assert!(head_held(&held, &clean), "the copy's head still reads whole");
        let state = tags(&mut Vec::new());
        assert_eq!(state.damage(), None);

        let mut notes = Vec::new();
        mend_copy(&state, &absent, Pass::AfterTaint, &mut notes);
        assert_eq!(std::fs::read(&copy_path).unwrap(), format!("{}\n", held).into_bytes());
        assert_eq!(std::fs::read(&ref_path).unwrap(), format!("{}\n", absent).into_bytes());
        assert!(notes.iter().any(|note| note.starts_with(&format!(
            "The copy of pallet \"@tags\"'s ref names {}, and was left as it is: the ref's head {} could not be read from the store:", held, absent
        ))), "{:?}", notes);
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
        // could not read, never a backup. Mutation: name the backup for every salvage but a found
        // one → the step is the backup, red.
        let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, "tags");
        std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
        std::fs::write(&ref_path, &one.as_bytes()[..12]).unwrap();
        let read = pallet_utils::get_meta_pallet_head("tags").unwrap_err();
        let step = crate::error::frames_in(&read).next().map(|(_, _, step)| step.to_string());
        assert_eq!(step.as_deref(), Some("forklift heal"), "{}", read);
    }

    // Where heal has no head it can rebuild a meta ref at, nothing named included, the refusal
    // names restoring the ref and its copy from a backup and nothing else; a refusal that is no
    // verdict on the ref names another step. Heal keeps the ref either way. One row per way a lost
    // or damaged @tags ref with no office reaches each. Mutations (each reaches its row's `owner`
    // assertion): read a damaged signature beside a parcel as a verdict → "a damaged signature, no
    // office" is Unrebuildable, red; read a descent the store said nothing of as its verdict → "the
    // copy's history back to the ref's bytes cannot be read" is Unrebuildable, red.
    #[test]
    fn a_meta_ref_heal_cannot_rebuild_names_only_a_backup() {
        use crate::util::meta_tree::{stack_meta_parcel, store_meta_tree};
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        let operator = |identifier: &str| crate::model::operator::Operator { identifier: identifier.to_string() };
        let parcel = |tree: String, parents: Vec<String>, by: &str, what: &str| stack_meta_parcel(tree, parents, &operator(by), what, None).unwrap();
        let tags = |what: &str| parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), "test", what);
        let object = |hash: &str| {
            let (folder, file) = crate::util::file_utils::get_path_for_object(hash).unwrap();
            std::path::PathBuf::from(folder).join(file)
        };
        let lost = b"not a head\n".to_vec();

        // Each row: its state, as the ref's bytes and the copy's (None: no copy), and whose heal
        // judges the refusal (None: not Unrebuildable, so no verdict on the ref).
        type Row = (Vec<u8>, Option<String>, Option<Owner>);
        let rows: Vec<(&str, Box<dyn Fn() -> Row>)> = vec![
            ("nothing names a head", Box::new(|| (lost.clone(), None, Some(Owner::Unrebuildable)))),
            ("the copy and the ref's bytes diverge", Box::new(|| {
                let (one, two) = (tags("one"), tags("two"));
                (two.as_bytes()[..12].to_vec(), Some(one), Some(Owner::Unrebuildable))
            })),
            ("the copy's history back to the ref's bytes is damaged", Box::new(|| {
                let (parent, salvaged) = (tags("parent"), tags("salvaged"));
                let child = parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), vec![parent.clone()], "test", "child");
                crate::util::file_utils::remove_file(&object(&parent)).unwrap();
                std::fs::write(object(&parent), b"no object").unwrap();
                (salvaged.as_bytes()[..12].to_vec(), Some(child), Some(Owner::Unrebuildable))
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
                (unreadable.as_bytes()[..12].to_vec(), None, Some(Owner::Unrebuildable))
            })),
            ("a tree of its history is absent", Box::new(|| {
                let tree = store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
                let head = parcel(tree.clone(), Vec::new(), "test", "absent");
                crate::util::file_utils::remove_file(&object(&tree)).unwrap();
                (lost.clone(), Some(head), Some(Owner::Unrebuildable))
            })),
            ("a parcel of its history has two parents", Box::new(|| {
                let tree = store_meta_tree(b"forklift-meta 1 tags", None).unwrap();
                let head = parcel(tree, vec![tags("one"), tags("two")], "test", "merge");
                (lost.clone(), Some(head), Some(Owner::Unrebuildable))
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
            // A folder that lists but cannot be searched: its lowercase entries are matched by
            // name, a mixed-case one only by reading it, which fails.
            #[cfg(unix)]
            ("the one match of the ref's bytes cannot be read", Box::new(|| {
                use std::os::unix::fs::PermissionsExt;
                let unreadable = format!("ab{}", "c".repeat(HASH_LENGTH - 2));
                let path = object(&unreadable);
                let folder = path.parent().unwrap();
                std::fs::create_dir_all(folder).unwrap();
                std::fs::write(folder.join(unreadable[2..].to_ascii_uppercase()), b"an object").unwrap();
                std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o444)).unwrap();
                (unreadable.as_bytes()[..12].to_vec(), None, None)
            })),
        ];

        for (row, state) in rows {
            let (_root, _scope) = scratch("ref-heal-verdicts");
            let (reference, copy, expected) = state();
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
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some(copy) = &copy {
                    let _ = std::fs::set_permissions(object(copy), std::fs::Permissions::from_mode(0o644));
                }
                let _ = std::fs::set_permissions(std::path::Path::new(&crate::util::file_utils::get_path_objects_root()).join("ab"), std::fs::Permissions::from_mode(0o755));
            }
            match &expected {
                Some(expected) => assert_eq!(&owner, expected, "{}: {}", row, judged.reason),
                None => assert_ne!(owner, Owner::Unrebuildable, "{}: {}", row, judged.reason),
            }
            let restore = "Restore the ref of @tags and its copy from a backup, then run \"forklift heal\" again.";
            assert_eq!(meta_step(judged, &refused) == restore, expected.is_some(), "{}: {}", row, meta_step(judged, &refused));
            assert_eq!(std::fs::read(&ref_path).unwrap(), reference, "{}: the ref is kept", row);
        }
    }

    // Which head descends is settled by the walk that reaches the other, whichever walk is longer
    // and whatever the other could not read: a ref one parcel ahead of a copy whose older history
    // cannot be read is ahead, not untold, and the copy one parcel ahead the same way. Only when
    // neither walk settles it does a failed read say why, on the side that failed. Mutation: let a
    // failed read decide at once, while the other walk can still settle it → the first
    // assertion reads `Err(true)`, red.
    #[test]
    fn the_head_that_descends_is_found_whatever_the_other_history_cannot_read() {
        use crate::util::meta_tree::{stack_meta_parcel, store_meta_tree};

        let (_root, _scope) = scratch("ref-heal-which-descends");
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        let parcel = |parents: Vec<String>, what: &str| {
            stack_meta_parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), parents, &operator, what, None).unwrap()
        };
        let object = |hash: &str| {
            let (folder, file) = crate::util::file_utils::get_path_for_object(hash).unwrap();
            std::path::PathBuf::from(folder).join(file)
        };
        let root = parcel(Vec::new(), "root");
        let middle = parcel(vec![root.clone()], "middle");
        let top = parcel(vec![middle.clone()], "top");
        let elsewhere = parcel(Vec::new(), "elsewhere");
        crate::util::file_utils::remove_file(&object(&root)).unwrap();
        std::fs::write(object(&root), b"no object").unwrap();

        assert_eq!(which_descends(&middle, &top).map_err(|(side, _)| side), Ok(Descent::RefAhead));
        assert_eq!(which_descends(&top, &middle).map_err(|(side, _)| side), Ok(Descent::CopyAhead));
        assert_eq!(which_descends(&middle, &elsewhere).map_err(|(side, _)| side), Err(true));
        assert_eq!(which_descends(&elsewhere, &middle).map_err(|(side, _)| side), Err(false));
        assert_eq!(which_descends(&elsewhere, &parcel(Vec::new(), "apart")).map_err(|(side, _)| side), Ok(Descent::Neither));
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

    // A stale meta ref's rebuild names the head the ref named before it writes, so the note is the
    // same on every outcome, in either pass: rebuilt, rebuilt though its folder sync failed once,
    // landed but not durable, written with its ref unread back, refused in a folder heal may not
    // write, and refused because the ref moved to another head before the hold. Each outcome is
    // reported outside the pass's deferrals, which a later pass may drop: the pass before the taint
    // recovery notes what it left for the pass after it. Mutations: name the head only from the
    // outcomes in which the write landed or may have → the not-written and moved rows carry no
    // note, red; name it after the write's match → the landed rows carry none, red; name it in the
    // landed arm after the landed-despite note → that row's order, red; note no deferral → the
    // refused rows of the first pass have no outcome outside the deferrals, red.
    #[test]
    fn a_stale_meta_ref_rebuild_names_the_head_it_named_whatever_the_write_does() {
        use crate::util::meta_tree::{stack_meta_parcel, store_meta_tree};
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        #[derive(Debug, Clone, Copy)]
        enum Row { Clean, #[cfg(unix)] LandedDespite, #[cfg(unix)] Undurable, Unread, #[cfg(unix)] ReadOnly, Moved }
        #[allow(unused_mut)]
        let mut rows = vec![Row::Clean, Row::Unread, Row::Moved];
        // A folder sync is a no-op off unix, so no injected sync failure can fire there; nor does
        // a 0o555 folder refuse a write.
        #[cfg(unix)]
        rows.extend([Row::LandedDespite, Row::Undurable, Row::ReadOnly]);
        const DEFERRAL: &str = "The pass before the taint recovery left the ref of pallet \"@tags\" for the pass after it: ";

        // Every row is run before the note is judged, so a failure names each row that misses it.
        let mut misnamed = Vec::new();
        for pass in [Pass::BeforeTaint, Pass::AfterTaint] {
            for row in rows.clone() {
                // A 0o555 folder does not refuse a write made as root.
                #[cfg(unix)]
                if matches!(row, Row::ReadOnly) && unsafe { libc::geteuid() } == 0 {
                    eprintln!("skipped the read-only row under {:?}: a read-only folder does not refuse root", pass);
                    continue;
                }
                // A root per row: a taint a row's fault leaves stands under its root for this
                // process.
                let (_root, _scope) = scratch(&format!("ref-heal-stale-gone-head-{:?}-{:?}", pass, row));
                let operator = crate::model::operator::Operator { identifier: "test".to_string() };
                let held = stack_meta_parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), &operator, "held", None).unwrap();
                let other = stack_meta_parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), &operator, "other", None).unwrap();
                let gone = format!("{}{}", &held[..63], if held.ends_with('0') { '1' } else { '0' });
                let ref_path = pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, TAGS_PALLET_NAME);
                let copy_path = pallet_utils::get_meta_copy_path(TAGS_PALLET_NAME);
                for path in [&ref_path, &copy_path] {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                }
                std::fs::write(&copy_path, format!("{}\n", held)).unwrap();
                std::fs::write(&ref_path, format!("{}\n", gone)).unwrap();
                #[cfg(unix)]
                let folder = ref_path.parent().unwrap().to_path_buf();
                let said = format!(
                    "The ref of pallet \"@tags\" named {}, which was not in the store, when heal set out to rebuild it at {} from the ref's copy.",
                    gone, held
                );

                let repair = {
                    let _sync: Option<crate::util::file_utils::SyncDirFaultGuard> = match row {
                        #[cfg(unix)]
                        Row::LandedDespite => Some(crate::util::file_utils::SyncDirFaultGuard::failing_exactly_once(&folder)),
                        #[cfg(unix)]
                        Row::Undurable => Some(crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&folder)),
                        _ => None,
                    };
                    #[cfg(unix)]
                    let _taint = matches!(row, Row::Undurable).then(crate::util::file_utils::TaintRecordFaultGuard::failing);
                    match row {
                        Row::Unread => READ_BACK_UNREAD.with(|unread| unread.set(usize::MAX)),
                        #[cfg(unix)]
                        Row::ReadOnly => {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
                        }
                        Row::Moved => {
                            let (moved, other) = (ref_path.clone(), other.clone());
                            pallet_utils::tests::set_before_hold(move || std::fs::write(&moved, format!("{}\n", other)).unwrap());
                        }
                        _ => {}
                    }
                    let repair = repair_meta(pass);
                    READ_BACK_UNREAD.with(|unread| unread.set(0));
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
                    }
                    repair.unwrap().repair
                };

                let naming: Vec<_> = repair.notes.iter().filter(|note| note.contains(&gone)).cloned().collect();
                if naming != vec![said.clone()] {
                    misnamed.push((pass, row, naming));
                }
                let rebuilt: Vec<_> = repair.rebuilt.iter().filter(|rebuilt| rebuilt.pallet == "@tags").map(|rebuilt| rebuilt.head.as_str()).collect();
                let refused = repair.refused.iter().find(|refused| refused.pallet == "@tags");
                let deferred = repair.deferred.iter().find(|refused| refused.pallet == "@tags");
                let deferrals: Vec<_> = repair.notes.iter().filter(|note| note.starts_with("The pass before the taint recovery left")).collect();
                // The refusal of a write this pass attempted: in the pass after the taint recovery
                // it is refused; in the pass before it, it is deferred and noted once, whose reason
                // starts as `reason` says, and so must be the whole note when `whole` is set.
                let not_landed = |reason: &str, whole: bool| match pass {
                    Pass::AfterTaint => {
                        let tags = refused.expect("@tags is refused");
                        assert!(tags.reason.starts_with(reason), "{:?} {:?}: {:?}", pass, row, tags);
                        assert!(deferrals.is_empty(), "{:?} {:?}: {:?}", pass, row, deferrals);
                        tags.meta.as_ref().unwrap().owner.clone()
                    }
                    Pass::BeforeTaint => {
                        let note = format!("{}{}", DEFERRAL, reason);
                        assert_eq!(deferrals.len(), 1, "{:?} {:?}: {:?}", pass, row, repair.notes);
                        match whole {
                            true => assert_eq!(deferrals[0], &format!("{}.", note), "{:?} {:?}", pass, row),
                            false => assert!(deferrals[0].starts_with(&note) && deferrals[0].ends_with('.') && !deferrals[0].ends_with(".."), "{:?} {:?}: {}", pass, row, deferrals[0]),
                        }
                        assert!(refused.is_none(), "{:?} {:?}: {:?}", pass, row, refused);
                        deferred.expect("@tags is deferred, so the pass after the taint recovery judges it again").meta.as_ref().unwrap().owner.clone()
                    }
                };
                match row {
                    Row::Clean => assert_eq!(rebuilt, vec![held.as_str()], "{:?} {:?}: {:?}", pass, row, repair),
                    #[cfg(unix)]
                    Row::LandedDespite => {
                        assert_eq!(rebuilt, vec![held.as_str()], "{:?} {:?}: {:?}", pass, row, repair);
                        let landed = repair.notes.iter().position(|note| note.starts_with("The ref of pallet \"@tags\" was rebuilt at"));
                        let named = repair.notes.iter().position(|note| *note == said);
                        assert!(landed.is_some(), "{:?}: the landed-despite arm ran: {:?}", pass, repair.notes);
                        assert!(named.is_some() && named < landed, "{:?}: the gone head is named before the write: {:?}", pass, repair.notes);
                    }
                    #[cfg(unix)]
                    Row::Undurable => assert_eq!(repair.undurable.len(), 1, "{:?} {:?}: {:?}", pass, row, repair),
                    Row::Unread => {
                        let reason = format!("the rebuild at {} was written, but its ref could not be read back: the read was refused for the test", held);
                        not_landed(&reason, true);
                    }
                    #[cfg(unix)]
                    Row::ReadOnly => {
                        let owner = not_landed(&format!("the rebuild at {} was not written: ", held), false);
                        assert_eq!(owner, Owner::Unwritten(None), "{:?}", pass);
                    }
                    Row::Moved => {
                        let owner = not_landed(&format!("the rebuild at {} was not written: ", held), false);
                        assert_eq!(owner, Owner::Again, "{:?}", pass);
                        assert_eq!(pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME).unwrap().as_deref(), Some(other.as_str()), "{:?}: nothing was written over the move", pass);
                    }
                }
                let landed = match row {
                    Row::Clean => true,
                    #[cfg(unix)]
                    Row::LandedDespite | Row::Undurable => true,
                    _ => false,
                };
                if landed {
                    assert!(deferrals.is_empty() && repair.refused.is_empty() && repair.deferred.is_empty(), "{:?} {:?}: {:?}", pass, row, repair);
                }
            }
        }
        assert!(misnamed.is_empty(), "rows whose notes do not name the gone head exactly once, as said: {:?}", misnamed);
    }

    // A head in a format this build does not know reads whole and is no verdict on the ref: a
    // newer Forklift may have written it. Its step is the upgrade. Mutation: judge a newer format
    // as the ref's own → the step is the backup, red.
    #[test]
    fn a_meta_head_in_a_newer_format_names_the_upgrade() {
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
        assert_eq!(std::fs::read(&ref_path).unwrap(), b"not a head\n", "the ref is kept");
    }

    // A parcel the store gives whole, its bytes matching its address, in a loose object version
    // this build does not know is no damage: a newer Forklift may have written it. Named by the
    // copy or by the ref's own bytes, its step is the upgrade. Mutation: file a parse failure of
    // verified bytes as damage (`Decided::Damaged`, `Salvage::Unknown`) → the ref's own, red.
    #[test]
    fn a_meta_head_in_a_newer_object_version_names_the_upgrade() {
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
            assert_eq!(std::fs::read(&ref_path).unwrap(), reference, "{}: the ref is kept", case);
        }
    }

    // A rebuild heal chose but could not write is no verdict on the ref, and a lasting fault
    // meets every rerun the same way: its step is to settle why, never heal again alone. Once
    // settled, heal rebuilds it. Mutation: file a failed write as `Again` → the step is
    // "forklift heal", red; as the ref's own → the step is the backup, red.
    #[cfg(unix)]
    #[test]
    fn a_meta_rebuild_heal_cannot_write_names_settling_why() {
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
        // The ref is lost beside a copy that names its head, in a folder heal may not write. A
        // 0o555 folder does not refuse a write made as root, so those cases are skipped there.
        let folder = ref_path.parent().unwrap().to_path_buf();
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped the read-only cases: a read-only folder does not refuse root");
        } else {
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();

            let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
            let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
            assert!(tags.reason.contains(&format!("the rebuild at {} was not written", parcel)), "{:?}", tags);
            assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Unwritten(None), "{:?}", tags);
            assert_eq!(meta_step(tags, &refused), "Settle why heal could not write the ref of @tags, as its refusal says, then run \"forklift heal\" again.");
            assert!(copy_path.exists(), "the copy is kept");

            // A write that failed may have landed: with its ref unread back, it is not said to be
            // unwritten, and its way out stays settling why. Mutation: judge the reason without the
            // read back → "was not written", red.
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
            READ_BACK_UNREAD.with(|unread| unread.set(usize::MAX));
            let refused = repair_meta(Pass::AfterTaint).unwrap().repair.refused;
            READ_BACK_UNREAD.with(|unread| unread.set(0));
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
            let tags = refused.iter().find(|refused| refused.pallet == "@tags").expect("@tags is refused");
            assert!(tags.reason.contains(&format!("the rebuild at {} may have been written: the write reported", parcel)), "{:?}", tags);
            assert!(tags.reason.contains("and its ref could not be read back: the read was refused for the test"), "{:?}", tags);
            assert_eq!(tags.meta.as_ref().unwrap().owner, Owner::Unwritten(None), "{:?}", tags);

            // The pass before the taint recovery defers the refusal and notes it too, for a lost
            // ref as for a stale one, since the pass after it may drop the deferral. Mutation:
            // note the deferral only for a stale ref → none here, red.
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
            let before = repair_meta(Pass::BeforeTaint).unwrap().repair;
            std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
            let deferral = format!("The pass before the taint recovery left the ref of pallet \"@tags\" for the pass after it: the rebuild at {} was not written: ", parcel);
            let deferrals: Vec<_> = before.notes.iter().filter(|note| note.starts_with("The pass before the taint recovery left")).collect();
            assert!(deferrals.len() == 1 && deferrals[0].starts_with(&deferral), "{:?}", before.notes);
            assert!(before.deferred.iter().any(|refused| refused.pallet == "@tags") && before.refused.is_empty(), "{:?}", before);
        }

        // Landed with its folder not synced and no taint recording that, and so again when heal
        // syncs it: the pass that wrote it, the first as heal runs them, says so, since the pass
        // after it reads @tags as undamaged. Mutation: let the landed read-back win without the
        // sync again → @tags is rebuilt, red.
        let (before, after) = {
            let _fault = crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&folder);
            let _taint = crate::util::file_utils::TaintRecordFaultGuard::failing();
            (repair_meta(Pass::BeforeTaint).unwrap().repair, repair_meta(Pass::AfterTaint).unwrap().repair)
        };
        assert!(before.rebuilt.is_empty() && before.deferred.is_empty(), "{:?}", before);
        let said = format!("The ref of pallet \"@tags\" was rebuilt at {}, but its ref is not durable: injected directory-sync failure for \"{}\"", parcel, folder.to_string_lossy());
        assert!(before.undurable.len() == 1 && before.undurable[0].starts_with(&said), "{:?}", before.undurable);
        assert!(after.rebuilt.is_empty() && after.refused.is_empty() && after.undurable.is_empty(), "{:?}", after);
        crate::util::file_utils::remove_file(&ref_path).unwrap();

        // The ref's write and sync held and only its copy's did not: the copy is said to be what
        // is not durable. Mutation: name the ref for every file `sync_again` syncs → "its ref", red.
        std::fs::write(&ref_path, &parcel.as_bytes()[..20]).unwrap();
        crate::util::file_utils::remove_file(&copy_path).unwrap();
        let copies = copy_path.parent().unwrap().to_path_buf();
        let copy = {
            let _fault = crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&copies);
            let _taint = crate::util::file_utils::TaintRecordFaultGuard::failing();
            repair_meta(Pass::BeforeTaint).unwrap().repair
        };
        let said = format!("The ref of pallet \"@tags\" was rebuilt at {}, but its copy is not durable: injected directory-sync failure for \"{}\"", parcel, copies.to_string_lossy());
        assert!(copy.undurable.len() == 1 && copy.undurable[0].starts_with(&said), "{:?}", copy);
        assert_eq!(pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME).unwrap().as_deref(), Some(parcel.as_str()));
        crate::util::file_utils::remove_file(&ref_path).unwrap();

        let rebuilt = repair_meta(Pass::AfterTaint).unwrap().repair;
        assert!(rebuilt.refused.is_empty(), "{:?}", rebuilt.refused);
        assert_eq!(pallet_utils::get_meta_pallet_head(TAGS_PALLET_NAME).unwrap().as_deref(), Some(parcel.as_str()));
    }

    // A ref write the pass before the taint recovery could not read back is read back once more,
    // at the write, and judged on that read, since the pass after the recovery would read a ref
    // it landed in as undamaged and judge nothing. Each row is a write, each column what that
    // second read finds.
    // Mutations: drop the settle in repair's arm, or judge there on the first read → a user
    // Landed cell judges the unread first read, red; the same in write_meta_rebuild, for every
    // meta ref or for the office or @redactions alone → that row's Landed cell is deferred on
    // the unread first read, red; drop the user unread note → the user Unread cells lack it,
    // red; skip the sync again on an unread settle → the kept-fault cells carry no undurable
    // entry, red.
    #[test]
    fn a_first_pass_write_it_could_not_read_back_is_read_again_at_the_write() {
        use crate::util::meta_tree::{stack_meta_parcel, store_meta_tree};
        use crate::util::office_utils::OFFICE_PALLET_NAME;
        use crate::util::redaction_utils::REDACTIONS_PALLET_NAME;
        use crate::util::tag_utils::TAGS_PALLET_NAME;

        // The rows that fail a write by an injected folder sync run on unix only: a folder sync
        // is a no-op elsewhere, so no injected sync failure can fire.
        #[cfg_attr(not(unix), allow(dead_code))]
        #[derive(Debug, Clone, Copy, PartialEq)]
        enum Row {
            /// A meta write that returned `Ok`.
            MetaOk,
            /// A meta write that left its folder unsynced, with no taint recording that, once.
            MetaUnrecorded,
            /// A meta write whose folder sync failed, and a taint records it.
            MetaFailed,
            /// As `MetaUnrecorded`, the sync failing every time.
            MetaKept,
            UserUnrecorded,
            UserFailed,
            UserKept,
            /// The office's write, beside two trust files that both verify it, so the trust
            /// repair stops the pass right after.
            Office,
            /// `@redactions`' write that returned `Ok`, beside a lost `@tags`.
            Redactions,
        }
        #[derive(Debug, Clone, Copy, PartialEq)]
        enum Read { Landed, Elsewhere, Unread }

        /// What a pass reported, the trust stop's report among them.
        #[derive(Debug)]
        struct Seen { rebuilt: Vec<(String, String)>, deferred: Vec<(String, String, Owner)>, refused: usize, notes: Vec<String>, undurable: Vec<String> }
        let deferred = |deferred: Vec<Refused>| deferred.into_iter()
            .map(|refused| (refused.pallet, refused.reason, refused.meta.expect("only a meta ref is deferred").owner))
            .collect::<Vec<_>>();
        let rebuilt = |rebuilt: Vec<Rebuilt>| rebuilt.into_iter().map(|rebuilt| (rebuilt.pallet, rebuilt.head)).collect::<Vec<_>>();

        let mut cells = Vec::new();
        for read in [Read::Landed, Read::Elsewhere, Read::Unread] {
            cells.extend([(Row::MetaOk, read), (Row::Redactions, read)]);
            #[cfg(unix)]
            cells.extend([(Row::MetaUnrecorded, read), (Row::MetaFailed, read), (Row::UserUnrecorded, read), (Row::UserFailed, read)]);
        }
        cells.extend([(Row::Office, Read::Landed), (Row::Office, Read::Unread)]);
        #[cfg(unix)]
        cells.extend([(Row::MetaKept, Read::Unread), (Row::UserKept, Read::Unread)]);

        const REFUSED_READ: &str = "the read was refused for the test";
        let operator = crate::model::operator::Operator { identifier: "test".to_string() };
        let meta_parcel = |message: &str| stack_meta_parcel(store_meta_tree(b"forklift-meta 1 tags", None).unwrap(), Vec::new(), &operator, message, None).unwrap();
        // A meta ref lost beside a copy that names `head`.
        let lose = |name: &str, head: &str| {
            let (reference, copy) = (pallet_utils::get_pallet_ref_path(PalletNamespace::Meta, name), pallet_utils::get_meta_copy_path(name));
            for path in [&reference, &copy] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            }
            std::fs::write(&copy, format!("{}\n", head)).unwrap();
            if reference.exists() {
                crate::util::file_utils::remove_file(&reference).unwrap();
            }
        };

        for (row, read) in cells {
            let cell = format!("{:?} {:?}", row, read);
            // A root per cell: a taint a cell's fault leaves stands under its root for this
            // process.
            let (fixture, _scratch) = match row {
                Row::Office => (Some(crate::util::audit_utils::tests::PrivilegeFixture::new(&format!("ref-heal-settle-{:?}", read))), None),
                _ => (None, Some(scratch(&format!("ref-heal-settle-{:?}-{:?}", row, read)))),
            };
            let user = matches!(row, Row::UserUnrecorded | Row::UserFailed | Row::UserKept);
            let failed = !matches!(row, Row::MetaOk | Row::Office | Row::Redactions);
            let kept = matches!(row, Row::MetaKept | Row::UserKept);
            let other = "0".repeat(64);

            let (namespace, name, head) = match (row, &fixture) {
                (Row::Office, Some(fixture)) => {
                    let head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().unwrap();
                    lose(OFFICE_PALLET_NAME, &head);
                    let resettled = crate::util::office_utils::encode_trust_anchor(&crate::util::office_utils::TrustAnchor {
                        enabled_at: fixture.anchor.enabled_at + 1, ..fixture.anchor.clone()
                    });
                    let copy = crate::globals::forklift_root().join(crate::util::office_utils::FILE_NAME_TRUST_COPY);
                    crate::util::file_utils::write_file_atomically(&copy, resettled.as_bytes()).unwrap();
                    (PalletNamespace::Meta, OFFICE_PALLET_NAME, head)
                }
                (Row::Redactions, _) => {
                    let head = redact("one");
                    lose(REDACTIONS_PALLET_NAME, &head);
                    lose(TAGS_PALLET_NAME, &meta_parcel("held"));
                    (PalletNamespace::Meta, REDACTIONS_PALLET_NAME, head)
                }
                _ if user => {
                    let head = stub_parcel("head");
                    let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, "main");
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    // Damaged, its bytes still naming the head.
                    std::fs::write(&path, &head.as_bytes()[..20]).unwrap();
                    (PalletNamespace::User, "main", head)
                }
                _ => {
                    let head = meta_parcel("held");
                    lose(TAGS_PALLET_NAME, &head);
                    (PalletNamespace::Meta, TAGS_PALLET_NAME, head)
                }
            };
            let wire = pallet_utils::PalletRef { namespace, name: name.to_string() }.to_wire();
            let ref_path = pallet_utils::get_pallet_ref_path(namespace, name);
            #[cfg(unix)]
            let folder = ref_path.parent().unwrap().to_path_buf();

            let seen = {
                #[cfg(unix)]
                let _sync = match row {
                    Row::MetaUnrecorded | Row::UserUnrecorded => Some(crate::util::file_utils::SyncDirFaultGuard::failing_exactly_once(&folder)),
                    Row::MetaFailed | Row::UserFailed | Row::MetaKept | Row::UserKept => Some(crate::util::file_utils::SyncDirFaultGuard::failing_exactly(&folder)),
                    Row::MetaOk | Row::Office | Row::Redactions => None,
                };
                #[cfg(unix)]
                let _taint = (failed && row != Row::MetaFailed && row != Row::UserFailed).then(crate::util::file_utils::TaintRecordFaultGuard::failing);
                // The first read back answers unread; the settle's reads the ref, or is unread too.
                READ_BACK_UNREAD.with(|unread| unread.set(if read == Read::Unread { 2 } else { 1 }));
                if read == Read::Elsewhere {
                    let (moved, other) = (ref_path.clone(), other.clone());
                    BEFORE_READ_BACK.with(|before| before.set(Some(Box::new(move || std::fs::write(&moved, format!("{}\n", other)).unwrap()))));
                }
                let seen = match row {
                    _ if user => {
                        let repair = repair(Pass::BeforeTaint).unwrap();
                        Seen { rebuilt: rebuilt(repair.rebuilt), deferred: deferred(repair.deferred), refused: repair.refused.len(), notes: repair.notes, undurable: repair.undurable }
                    }
                    Row::Office => {
                        let stopped = repair_meta(Pass::BeforeTaint).expect_err("the trust repair stops the pass");
                        Seen { rebuilt: rebuilt(stopped.rebuilt), deferred: deferred(stopped.deferred), refused: stopped.refused.len(), notes: stopped.notes, undurable: Vec::new() }
                    }
                    _ => {
                        let repair = repair_meta(Pass::BeforeTaint).unwrap().repair;
                        Seen { rebuilt: rebuilt(repair.rebuilt), deferred: deferred(repair.deferred), refused: repair.refused.len(), notes: repair.notes, undurable: repair.undurable }
                    }
                };
                let left = READ_BACK_UNREAD.with(|unread| unread.replace(0));
                let unrun = BEFORE_READ_BACK.with(std::cell::Cell::take).is_some();
                assert!(left == 0 && !unrun, "{}: the write was read back twice: {:?}", cell, seen);
                seen
            };

            assert_eq!(seen.refused, 0, "{}: the first pass refuses nothing: {:?}", cell, seen);
            let ours = seen.deferred.iter().find(|deferred| deferred.0 == wire);
            let deferral = seen.notes.iter().filter(|note| note.starts_with(&format!("The pass before the taint recovery left the ref of pallet \"{}\"", wire))).collect::<Vec<_>>();
            let rebuilt_here = seen.rebuilt.contains(&(wire.clone(), head.clone()));
            let maybe_note = format!("The ref of pallet \"{}\" may have been rebuilt at {}: the write reported ", wire, head);
            let maybe: Vec<_> = seen.notes.iter().filter(|note| note.starts_with(&maybe_note)).collect();
            if read != Read::Unread {
                let first = seen.notes.iter().chain(seen.deferred.iter().map(|deferred| &deferred.1)).find(|said| said.contains(REFUSED_READ));
                assert!(first.is_none(), "{}: the first read's answer is judged on: {:?}", cell, first);
            }

            match read {
                Read::Landed => {
                    assert!(rebuilt_here && ours.is_none() && deferral.is_empty(), "{}: rebuilt, not deferred: {:?}", cell, seen);
                    let despite = format!("The ref of pallet \"{}\" was rebuilt at {}, though its write reported: ", wire, head);
                    assert_eq!(seen.notes.iter().any(|note| note.starts_with(&despite)), failed, "{}: {:?}", cell, seen.notes);
                    assert!(seen.undurable.is_empty(), "{}: {:?}", cell, seen.undurable);
                }
                Read::Elsewhere if user => {
                    assert!(seen.rebuilt.is_empty() && seen.deferred.is_empty() && maybe.is_empty() && seen.undurable.is_empty(), "{}: nothing landed, so nothing is said: {:?}", cell, seen);
                }
                Read::Elsewhere => {
                    let (reason, owner) = ours.map(|ours| (ours.1.clone(), ours.2.clone())).unwrap_or_else(|| panic!("{}: deferred: {:?}", cell, seen));
                    match failed {
                        false => {
                            assert_eq!((reason.as_str(), &owner), (format!("the rebuild at {} did not read back", head).as_str(), &Owner::Again), "{}", cell);
                            assert_eq!(deferral, vec![&format!("The pass before the taint recovery left the ref of pallet \"{}\" for the pass after it: the rebuild at {} did not read back.", wire, head)], "{}", cell);
                        }
                        true => {
                            assert!(reason.starts_with(&format!("the rebuild at {} was not written: ", head)), "{}: {}", cell, reason);
                            assert_eq!(owner, Owner::Unwritten(None), "{}", cell);
                        }
                    }
                    assert!(!rebuilt_here && seen.undurable.is_empty(), "{}: {:?}", cell, seen);
                }
                Read::Unread if user => {
                    assert_eq!(maybe.len(), 1, "{}: the head heal set out to write is named once: {:?}", cell, seen.notes);
                    assert!(maybe[0].ends_with(&format!(", and its ref could not be read back: {}.", REFUSED_READ)), "{}: {}", cell, maybe[0]);
                    assert!(seen.rebuilt.is_empty() && seen.deferred.is_empty(), "{}: {:?}", cell, seen);
                }
                Read::Unread => {
                    let (reason, owner) = ours.map(|ours| (ours.1.clone(), ours.2.clone())).unwrap_or_else(|| panic!("{}: deferred: {:?}", cell, seen));
                    match failed {
                        false => assert_eq!(reason, format!("the rebuild at {} was written, but its ref could not be read back: {}", head, REFUSED_READ), "{}", cell),
                        true => assert!(reason.starts_with(&format!("the rebuild at {} may have been written: the write reported ", head))
                            && reason.ends_with(&format!(", and its ref could not be read back: {}", REFUSED_READ)), "{}: {}", cell, reason),
                    }
                    assert_eq!(owner, Owner::Unwritten(None), "{}", cell);
                    assert_eq!(deferral.len(), 1, "{}: {:?}", cell, seen.notes);
                    assert!(!rebuilt_here, "{}: {:?}", cell, seen);
                }
            }

            // Not durable if it landed: said only where the sync again fails too, and only while
            // the ref still cannot be read.
            #[cfg(unix)]
            match kept {
                true => {
                    let said = format!(
                        "The ref of pallet \"{}\" may have been rebuilt at {}, and if it was, its ref is not durable: injected directory-sync failure for \"{}\"",
                        wire, head, folder.to_string_lossy()
                    );
                    assert!(seen.undurable.len() == 1 && seen.undurable[0].starts_with(&said), "{}: {:?}", cell, seen.undurable);
                }
                false => assert!(seen.undurable.is_empty(), "{}: {:?}", cell, seen.undurable),
            }
            #[cfg(not(unix))]
            let _ = kept;

            // A @tags beside @redactions waits for it, as long as heal cannot tell @redactions
            // stands.
            if row == Row::Redactions {
                let tags = seen.deferred.iter().find(|deferred| deferred.0 == "@tags");
                match read {
                    Read::Landed => assert!(tags.is_none() && seen.rebuilt.iter().any(|rebuilt| rebuilt.0 == "@tags"), "{}: {:?}", cell, seen),
                    Read::Elsewhere | Read::Unread => assert_eq!(tags.map(|tags| (tags.1.as_str(), &tags.2)), Some((
                        "it is verified against the office and checked against the redactions, and the ref of pallet \"@redactions\" is damaged too: set that first",
                        &Owner::Cascade("@redactions".to_string()),
                    )), "{}: {:?}", cell, seen),
                }
            }
        }

        // The settle answers each attempt with what its ref reads as now.
        let (_root, _scope) = scratch("ref-heal-settle-answers");
        let head = stub_parcel("head");
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, "main");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\n", head)).unwrap();
        let attempt = |name: &str| WriteAttempt::Ref { namespace: PalletNamespace::User, name: name.to_string(), head: head.clone(), error: None };
        let mut report = Repair::default();
        let answers = |settled: Vec<(WriteAttempt, ReadBack)>| settled.into_iter().map(|(WriteAttempt::Ref { name, head, .. }, read)| (name, head, match read {
            ReadBack::Landed => "landed".to_string(),
            ReadBack::Elsewhere => "elsewhere".to_string(),
            ReadBack::Unread(why) => why,
        })).collect::<Vec<_>>();
        assert_eq!(answers(settle_attempts(vec![attempt("main"), attempt("absent")], &mut report)), vec![
            ("main".to_string(), head.clone(), "landed".to_string()),
            ("absent".to_string(), head.clone(), "elsewhere".to_string()),
        ]);
        READ_BACK_UNREAD.with(|unread| unread.set(1));
        assert_eq!(answers(settle_attempts(vec![attempt("main")], &mut report)), vec![("main".to_string(), head.clone(), REFUSED_READ.to_string())]);
        assert_eq!(report, Repair::default(), "an attempt that returned Ok needs no sync again");
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
