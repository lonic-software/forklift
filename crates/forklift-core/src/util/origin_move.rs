//! The intent of a move of a partial warehouse's origin, and the check every writer of what a move
//! verifies makes against it.
//!
//! A move points a partial warehouse at a new origin. Before it rewrites the origin it checks that
//! the new server holds everything this warehouse leans on the origin for: every head in the
//! fetched-heads ledger, every entry of the held-heads record and every parcel of the sealed-object
//! record. That check talks to the network, so it runs without the
//! [`RefLock`](crate::util::lock_utils::RefLock), and a write to one of those records meanwhile
//! could add something the check never saw. So the move first writes an intent, under the ref
//! lock, and from then until it clears the intent every writer of those records refuses, checking
//! in the same hold as its write: the ledger's writer, the held-heads writer and every write of the
//! sealed-object record (an append, a compaction or bound, a rebuild), and a pallet's removal.
//! Every remote client refuses up front too ([`origin_utils::checked_remote`]), so a fetch, a lift
//! or a read of the remote's heads stops before it reaches the network. Stack and the local reads
//! go on.
//!
//! The intent is a [`record_utils`] file in the shared forklift root holding the move's owner
//! token, its phase and the server it moves to. In the checking phase nothing has been rewritten,
//! so a failed or aborted move clears it; once the move starts rewriting, the intent is cleared
//! only by a move that completes. The token is what lets a move tell its own intent from one a
//! killed move left: the [`MoveLock`] says whether any move is running, and the token says which
//! one wrote the intent. An intent file that does not parse is read as a move that may have
//! started rewriting: every writer refuses as under one, and the next move finishes it. One a
//! newer Forklift wrote, in a version this build does not know, is refused and never taken over.

use std::path::PathBuf;
use crate::error::{CoreError, RefusalCode};
use crate::globals;
use crate::util::lock_utils::{MoveLock, RefLock};
use crate::util::{file_utils, origin_utils, record_utils, sign_utils};

/// The intent's file name in the shared forklift root, and its [`record_utils`] kind and version.
pub const FILE_NAME: &str = "origin-move";
const RECORD: &str = "origin-move";
const VERSION: u32 = 2;

/// How far a move has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The move checks the new server and has rewritten nothing; failing or aborting clears it.
    Checking,
    /// The move has started rewriting the origin; only a completed move clears it.
    Rewriting,
}

impl Phase {
    const ALL: [Phase; 2] = [Phase::Checking, Phase::Rewriting];

    fn as_str(self) -> &'static str {
        match self {
            Phase::Checking => "checking",
            Phase::Rewriting => "rewriting",
        }
    }

    fn parse(name: &str) -> Option<Phase> {
        Phase::ALL.into_iter().find(|phase| phase.as_str() == name)
    }
}

/// A recorded intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// The owner token of the move that wrote it, as hex.
    pub token: String,

    /// How far that move got.
    pub phase: Phase,

    /// The URL of the server the move takes as the new origin, once it has been checked; none for
    /// a move that took over a damaged intent and has not checked its server yet.
    pub target: Option<String>,
}

/// Proof that the holder owns the recorded intent: only [`begin`] makes one, under the
/// [`MoveLock`].
#[derive(Debug, PartialEq, Eq)]
pub struct MoveToken(String);

fn intent_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME)
}

/// The intent file as read: absent, an intent, damage, or a version this build does not know, the
/// last two described as the rest of a sentence that opens with the file's path.
enum Recorded {
    Absent,
    Intent(Intent),
    Damaged(String),
    Newer(String),
}

fn read_recorded() -> Result<Recorded, String> {
    let path = intent_path();

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Recorded::Absent),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let entries = match record_utils::from_bytes(&bytes, RECORD, VERSION) {
        Ok(entries) => entries,
        Err(e) => {
            let newer = match &e {
                record_utils::RecordError::OtherVersion(header) => header
                    .strip_prefix(&format!("/forklift-{} ", RECORD))
                    // Only a version this record's writers could have written: digits alone, no
                    // leading zero, which `parse` would otherwise accept along with a sign.
                    .filter(|version| version.bytes().all(|b| b.is_ascii_digit()) && !version.starts_with('0'))
                    .and_then(|version| version.parse::<u32>().ok())
                    .is_some_and(|version| version > VERSION),
                _ => false,
            };

            return Ok(if newer { Recorded::Newer(e.describe()) } else { Recorded::Damaged(e.describe()) });
        }
    };

    if !(2..=3).contains(&entries.len()) {
        return Ok(Recorded::Damaged(format!(
            "holds {} entries where it should hold a token, a phase and perhaps a target.", entries.len()
        )));
    }

    let mut entries = entries.into_iter();
    let (token, phase, target) = (entries.next().unwrap(), entries.next().unwrap(), entries.next());

    let token = match token.strip_prefix("token ") {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => hex.to_string(),
        _ => return Ok(Recorded::Damaged("does not open with a move's owner token.".to_string())),
    };

    let Some(phase) = phase.strip_prefix("phase ").and_then(Phase::parse) else {
        return Ok(Recorded::Damaged("names no phase a move has.".to_string()));
    };

    let target = match target.as_deref().map(|target| target.strip_prefix("target ")) {
        None => None,
        Some(Some(url)) if !url.is_empty() => Some(url.to_string()),
        Some(_) => return Ok(Recorded::Damaged("names no server the move takes as the origin.".to_string())),
    };

    if target.is_none() && phase == Phase::Checking {
        return Ok(Recorded::Damaged("names no server its move checks.".to_string()));
    }

    Ok(Recorded::Intent(Intent { token, phase, target }))
}

fn damaged_message(what: &str) -> String {
    format!("The file \"{}\" {}", intent_path().to_string_lossy(), what)
}

/// The intent recorded in the warehouse in scope, or none when the file is absent. Lock-free: the
/// file is replaced atomically. A file that does not parse is an error, never no intent.
pub fn read_intent() -> Result<Option<Intent>, String> {
    match read_recorded()? {
        Recorded::Absent => Ok(None),
        Recorded::Intent(intent) => Ok(Some(intent)),
        Recorded::Damaged(what) | Recorded::Newer(what) => Err(damaged_message(&what)),
    }
}

fn write_intent(_lock: &RefLock, intent: &Intent) -> Result<(), String> {
    let entries = [format!("token {}", intent.token), format!("phase {}", intent.phase.as_str())]
        .into_iter()
        .chain(intent.target.iter().map(|target| format!("target {}", target)));

    file_utils::replace_file_atomically(&intent_path(), &record_utils::to_bytes(RECORD, VERSION, entries))
        .map_err(|e| format!("Error while recording the move of this warehouse's origin: {}", e))
}

fn remove_intent(_lock: &RefLock) -> Result<(), String> {
    let path = intent_path();

    match std::fs::remove_file(&path) {
        Ok(()) => file_utils::sync_dir(&globals::forklift_root()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Error while clearing \"{}\": {}", path.to_string_lossy(), e)),
    }
}

fn new_token() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    sign_utils::to_hex(&bytes)
}

/// Take ownership of the intent, for a move to `target` that holds the [`MoveLock`]: with none
/// recorded, record one in the checking phase; one in the checking phase was left by a move that
/// is no longer running (the lock says so), and is replaced; one in the rewriting phase is adopted
/// with its phase, which is never downgraded, since the origin may already be partly rewritten,
/// and keeps its target until this move has checked its own ([`set_rewriting`]). A damaged one is
/// read as rewriting and replaced by a rewriting intent naming no target, since no one can tell
/// how far its move got or where it went. A move that starts rewriting still checks its own target
/// first: the phase says only what may already be rewritten, not what was checked, and the origin
/// is rewritten only to the target recorded once it has been ([`require_rewriting`]).
///
/// # Returns
/// * `Ok((MoveToken, Phase))` - This move's token, and the phase it starts in.
/// * `Err(String)`            - If the intent cannot be read or written, or a newer Forklift wrote
///   it, which only that Forklift may finish.
pub fn begin(_move_lock: &MoveLock, lock: &RefLock, target: &str) -> Result<(MoveToken, Phase), String> {
    let intent = match read_recorded()? {
        Recorded::Intent(intent) if intent.phase == Phase::Rewriting => return Ok((MoveToken(intent.token), Phase::Rewriting)),
        Recorded::Newer(what) => return Err(String::from(newer_refusal(&what))),
        Recorded::Damaged(_) => Intent { token: new_token(), phase: Phase::Rewriting, target: None },
        Recorded::Absent | Recorded::Intent(_) => Intent { token: new_token(), phase: Phase::Checking, target: Some(target.to_string()) },
    };

    write_intent(lock, &intent)?;

    Ok((MoveToken(intent.token), intent.phase))
}

/// Read the intent and confirm `token` owns it.
fn owned(token: &MoveToken) -> Result<Intent, String> {
    match read_recorded()? {
        Recorded::Intent(intent) if intent.token == token.0 => Ok(intent),
        Recorded::Damaged(what) => Err(String::from(damaged_refusal(&what))),
        Recorded::Newer(what) => Err(String::from(newer_refusal(&what))),
        Recorded::Absent | Recorded::Intent(_) => Err("This move no longer owns the record of the origin's move; nothing was rewritten. \
            Run the move again.".to_string()),
    }
}

/// Mark the move as rewriting to `target`, for the move that owns the intent once it has checked
/// `target`: from here on only a completed move clears it, and the refusals name `target`.
pub fn set_rewriting(lock: &RefLock, token: &MoveToken, target: &str) -> Result<(), String> {
    let intent = owned(token)?;

    if intent.phase == Phase::Rewriting && intent.target.as_deref() == Some(target) {
        return Ok(());
    }

    write_intent(lock, &Intent { phase: Phase::Rewriting, target: Some(target.to_string()), ..intent })
}

/// Confirm that `token` owns the intent, that the move is rewriting, and that `url` names the
/// target it recorded when it did ([`origin_utils::same_remote`]): the one state in which the
/// origin may be rewritten to `url`.
pub fn require_rewriting(_lock: &RefLock, token: &MoveToken, url: &str) -> Result<(), String> {
    let intent = owned(token)?;

    let why = match (intent.phase, intent.target.as_deref()) {
        (Phase::Checking, _) => "the move has not started rewriting it".to_string(),
        (Phase::Rewriting, None) => "the move has not checked a server to rewrite it to".to_string(),
        (Phase::Rewriting, Some(target)) if origin_utils::same_remote(target, url) => return Ok(()),
        (Phase::Rewriting, Some(target)) => format!("the move is rewriting it to \"{}\"", target),
    };

    Err(format!("The origin was not rewritten to \"{}\": {}.", url, why))
}

/// Clear the intent of a move that completed: the origin is rewritten.
pub fn finish(lock: &RefLock, token: &MoveToken) -> Result<(), String> {
    owned(token)?;
    remove_intent(lock)
}

/// Clear the intent of a move that failed, when it is still in the checking phase; a rewriting one
/// stays for the next move to finish.
///
/// # Returns
/// * `Ok(true)`    - The intent was cleared.
/// * `Ok(false)`   - It is rewriting, and stays.
/// * `Err(String)` - If the intent cannot be read, is not this move's, or cannot be removed.
pub fn abandon(lock: &RefLock, token: &MoveToken) -> Result<bool, String> {
    match owned(token)?.phase {
        Phase::Checking => remove_intent(lock).map(|()| true),
        Phase::Rewriting => Ok(false),
    }
}

/// Clear an intent a move left in the checking phase, for `--abort`, which holds the
/// [`MoveLock`], so no move is running.
///
/// # Returns
/// * `Ok(Some(intent))` - The intent found: cleared when it was checking, kept when rewriting.
/// * `Ok(None)`         - No intent is recorded.
/// * `Err(String)`      - If the intent cannot be read or removed, or is damaged, which is kept
///   and named with the move that finishes it.
pub fn abort(_move_lock: &MoveLock, lock: &RefLock) -> Result<Option<Intent>, String> {
    let intent = match read_recorded()? {
        Recorded::Absent => return Ok(None),
        Recorded::Intent(intent) => intent,
        Recorded::Damaged(what) => return Err(String::from(damaged_refusal(&what))),
        Recorded::Newer(what) => return Err(String::from(newer_refusal(&what))),
    };

    if intent.phase == Phase::Checking {
        remove_intent(lock)?;
    }

    Ok(Some(intent))
}

/// The `origin_move_pending` refusal for a command that would fetch, talk to the remote or write
/// a record a move checks, while `intent` is recorded.
pub fn pending_refusal(intent: &Intent) -> CoreError {
    let Some(target) = &intent.target else {
        return damaged_refusal("was taken over from a damaged record by a move that has not checked its server.");
    };
    let finish = format!("Finish the move with \"forklift remote move {}\"", target);
    let next_step = match intent.phase {
        Phase::Checking => format!("{}, or abandon it with \"forklift remote move --abort\".", finish),
        Phase::Rewriting => format!("{}; it has started rewriting the origin, so it cannot be abandoned.", finish),
    };

    CoreError::refusal(
        RefusalCode::OriginMovePending,
        format!(
            "A move of this warehouse's origin to \"{}\" has not finished (it was {} when it stopped), \
            so nothing fetches, talks to the remote or changes what the move checks until it does. {}",
            target, intent.phase.as_str(), next_step
        ),
        next_step,
    )
}

/// The `origin_move_pending` refusal for a damaged intent, `what` being the rest of a sentence
/// opening with its path: read as a move that may have started rewriting, so no abort clears it.
fn damaged_refusal(what: &str) -> CoreError {
    let next_step = "Finish the move with \"forklift remote move <url>\", naming any server that \
        holds this warehouse's content, such as the one the move was going to; it may have started \
        rewriting the origin, so it cannot be abandoned.";

    CoreError::refusal(
        RefusalCode::OriginMovePending,
        format!(
            "{} It records a move of this warehouse's origin, and how far that move got cannot be \
            told, so nothing fetches, talks to the remote or changes what the move checks until a \
            move finishes. {}",
            damaged_message(what), next_step
        ),
        next_step,
    )
}

/// The `origin_move_pending` refusal for an intent a newer Forklift wrote, `what` being the rest
/// of a sentence opening with its path: this build cannot tell how far that move got, and never
/// takes it over.
fn newer_refusal(what: &str) -> CoreError {
    let next_step = "Finish or abandon the move with the newer Forklift that started it.";

    CoreError::refusal(
        RefusalCode::OriginMovePending,
        format!(
            "{} It records a move of this warehouse's origin that a newer Forklift started, so \
            nothing fetches, talks to the remote or changes what the move checks until it \
            finishes. {}",
            damaged_message(what), next_step
        ),
        next_step,
    )
}

/// Refuse while a move's intent is recorded, or a damaged or newer one, for a command about to
/// fetch or talk to the remote, before it does. Lock-free, so a writer re-checks in its own hold
/// ([`refuse_if_pending`]).
pub fn ensure_none_pending() -> Result<(), CoreError> {
    match read_recorded()? {
        Recorded::Absent => Ok(()),
        Recorded::Intent(intent) => Err(pending_refusal(&intent)),
        Recorded::Damaged(what) => Err(damaged_refusal(&what)),
        Recorded::Newer(what) => Err(newer_refusal(&what)),
    }
}

/// The check a writer of a record a move checks makes in the ref-lock hold of its write, before
/// it writes: an `origin_move_pending` refusal, framed so it keeps its code through a `String`,
/// while a move's intent is recorded.
pub fn refuse_if_pending(_lock: &RefLock) -> Result<(), String> {
    ensure_none_pending().map_err(String::from)
}

/// Record an intent in `phase` for a test fixture, as a move that took the locks would, and
/// return its token.
#[doc(hidden)]
pub fn begin_for_tests(phase: Phase) -> MoveToken {
    let move_lock = MoveLock::try_acquire().unwrap().expect("no move runs in a test fixture");
    let lock = RefLock::acquire().unwrap();
    let (token, _) = begin(&move_lock, &lock, "http://new-origin.example").unwrap();

    if phase == Phase::Rewriting {
        set_rewriting(&lock, &token, "http://new-origin.example").unwrap();
    }

    token
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch {
        root: PathBuf,
        _scope: globals::StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-origin-move-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = globals::StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn refusal_code(error: String) -> Option<RefusalCode> {
        match CoreError::from(error) {
            CoreError::Refusal { code, .. } => Some(code),
            CoreError::Other(_) => None,
        }
    }

    /// A move records a checking intent; a second move, once the first is gone, replaces a
    /// checking one with its own token and adopts a rewriting one with its phase, keeping its
    /// target until the adopting move has checked its own; a failure and an abort clear only a
    /// checking intent, and a completed move clears either. A damaged intent is taken over as
    /// rewriting with no target until its move checks one, and an abort keeps it; one a newer
    /// Forklift wrote is never taken over, while an older version reads as damage.
    ///
    /// Revert: replace a rewriting intent in `begin` → the adoption row gets a new token in the
    /// checking phase; or let `abandon`/`abort` clear a rewriting intent → it is gone; or record the
    /// adopting move's target in `begin` → the kept-target row names it; or read a damaged intent
    /// as checking → the takeover row starts checking; or take a newer intent over as damaged →
    /// its `begin` succeeds; or read every other version as newer → the older row's `begin` fails.
    #[test]
    fn a_move_owns_its_intent_and_a_rewriting_one_is_never_downgraded() {
        let _scratch = Scratch::new("own");
        assert_eq!(read_intent().unwrap(), None);

        let first = begin_for_tests(Phase::Checking);
        let recorded = read_intent().unwrap().unwrap();
        assert_eq!((recorded.token.as_str(), recorded.phase), (first.0.as_str(), Phase::Checking));

        // The next move (the first one's locks are gone) replaces a checking intent.
        let move_lock = MoveLock::try_acquire().unwrap().unwrap();
        assert!(MoveLock::try_acquire().unwrap().is_none(), "a second move refuses on the lock");
        let lock = RefLock::acquire().unwrap();
        let (second, phase) = begin(&move_lock, &lock, "http://second.example").unwrap();
        assert_eq!(phase, Phase::Checking);
        assert_ne!(second, first, "a replaced intent gets a new token");
        assert!(set_rewriting(&lock, &first, "http://first.example").is_err(), "the replaced move no longer owns it");
        assert_eq!(read_intent().unwrap().unwrap().phase, Phase::Checking);

        set_rewriting(&lock, &second, "http://second.example").unwrap();
        assert_eq!(abandon(&lock, &second).unwrap(), false, "a failure keeps a rewriting intent");
        assert_eq!(abort(&move_lock, &lock).unwrap().map(|intent| intent.phase), Some(Phase::Rewriting));
        assert_eq!(read_intent().unwrap().unwrap().phase, Phase::Rewriting, "an abort keeps it too");

        let (adopted, phase) = begin(&move_lock, &lock, "http://third.example").unwrap();
        assert_eq!((adopted.0.as_str(), phase), (second.0.as_str(), Phase::Rewriting), "adopted with its phase");
        assert_eq!(read_intent().unwrap().unwrap().target.as_deref(), Some("http://second.example"), "the target stays until checked");
        set_rewriting(&lock, &adopted, "http://third.example").unwrap();
        assert_eq!(read_intent().unwrap().unwrap().target.as_deref(), Some("http://third.example"), "and is switched once it is");

        finish(&lock, &adopted).unwrap();
        assert_eq!(read_intent().unwrap(), None);

        let (third, _) = begin(&move_lock, &lock, "http://third.example").unwrap();
        assert_eq!(abandon(&lock, &third).unwrap(), true, "a failure clears a checking intent");
        assert_eq!(read_intent().unwrap(), None);
        begin(&move_lock, &lock, "http://third.example").unwrap();
        assert_eq!(abort(&move_lock, &lock).unwrap().map(|intent| intent.phase), Some(Phase::Checking));
        assert_eq!(read_intent().unwrap(), None);
        assert_eq!(abort(&move_lock, &lock).unwrap(), None);

        // No one can tell how far the move behind a damaged intent got: an abort keeps it, naming
        // the move that finishes it, and the next move takes it over as rewriting.
        std::fs::write(intent_path(), record_utils::to_bytes(RECORD, VERSION, ["token ab"])).unwrap();
        let kept = abort(&move_lock, &lock).unwrap_err();
        assert!(kept.contains("remote move <url>") && kept.contains(FILE_NAME), "{}", kept);
        assert_eq!(refusal_code(kept), Some(RefusalCode::OriginMovePending));
        assert!(read_intent().is_err(), "the abort keeps a damaged intent");
        let (taken, phase) = begin(&move_lock, &lock, "http://fourth.example").unwrap();
        assert_eq!(phase, Phase::Rewriting);
        let recorded = read_intent().unwrap().unwrap();
        assert_eq!((recorded.phase, recorded.target), (Phase::Rewriting, None), "no target until one is checked");
        let pending = String::from(ensure_none_pending().unwrap_err());
        assert!(pending.contains("remote move <url>") && !pending.contains("fourth"), "{}", pending);
        let error = require_rewriting(&lock, &taken, "http://fourth.example").unwrap_err();
        assert!(error.contains("not checked a server"), "{}", error);
        set_rewriting(&lock, &taken, "http://fourth.example").unwrap();
        require_rewriting(&lock, &taken, "http://fourth.example").unwrap();
        finish(&lock, &taken).unwrap();

        // An older version is damage, taken over as such.
        let older = record_utils::to_bytes(RECORD, VERSION - 1, ["token ab", "phase checking"]);
        std::fs::write(intent_path(), &older).unwrap();
        assert_eq!(begin(&move_lock, &lock, "http://fifth.example").unwrap().1, Phase::Rewriting);

        // An intent in a version this build does not know is a newer Forklift's move: refused,
        // naming that, and left as it is by a move and an abort alike.
        let newer = record_utils::to_bytes(RECORD, VERSION + 1, ["token ab", "phase checking", "target http://x"]);
        std::fs::write(intent_path(), &newer).unwrap();
        for error in [begin(&move_lock, &lock, "http://fifth.example").unwrap_err(), abort(&move_lock, &lock).unwrap_err()] {
            assert!(error.contains("newer Forklift"), "{}", error);
            assert_eq!(refusal_code(error), Some(RefusalCode::OriginMovePending));
        }
        assert_eq!(std::fs::read(intent_path()).unwrap(), newer, "a newer intent is never rewritten");

        // A version no writer spells that way is damage, not a newer Forklift's move.
        for version in ["+3", "03"] {
            let mut odd = format!("/forklift-{} {}\n", RECORD, version).into_bytes();
            odd.extend_from_slice(&newer[newer.iter().position(|&b| b == b'\n').unwrap() + 1..]);
            std::fs::write(intent_path(), &odd).unwrap();
            assert_eq!(begin(&move_lock, &lock, "http://fifth.example").unwrap().1, Phase::Rewriting, "{}", version);
        }
    }

    /// Under an intent in either phase, every writer of a record a move checks refuses with
    /// `origin_move_pending` and leaves its file as it was: the ledger (written whole and by
    /// `record_fetch`), the held-heads record, and the sealed-object record's append, compaction
    /// and rebuild. Once the intent is cleared, the same writes land, so each refusal was the
    /// funnel and not the write failing for another reason.
    ///
    /// Revert: drop the check from any one writer → that row writes and its file changes → red.
    #[test]
    fn every_funnel_refuses_under_an_intent_and_writes_nothing() {
        use crate::util::{fetch_ledger, held_heads, origin_utils, sealed_utils};
        use crate::util::sealed_utils::SealOp;

        let _scratch = Scratch::new("funnels");
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        let forklift = globals::forklift_root();
        origin_utils::write_for_tests("http://origin.example");

        // A record and its removal, so a compaction has something to rewrite.
        sealed_utils::append(&[(SealOp::Record, &a), (SealOp::Remove, &a)]).unwrap();
        let sealed_before = std::fs::read(sealed_utils::sealed_path()).unwrap();
        // A record without its header is damage, which a rebuild rewrites.
        let damaged = sealed_utils::encode_frame(SealOp::Record, &a).unwrap();

        let refused = |error: String, what: &str| {
            assert_eq!(refusal_code(error.clone()), Some(RefusalCode::OriginMovePending), "{}: {}", what, error);
        };
        let read = |name: &str| std::fs::read(forklift.join(name)).ok();

        for phase in [Phase::Checking, Phase::Rewriting] {
            let token = begin_for_tests(phase);

            {
                let lock = RefLock::acquire().unwrap();
                let entry = fetch_ledger::LedgerEntry { head: a.clone(), pallet: "main".to_string() };
                refused(fetch_ledger::write_ledger(&lock, &[entry]).unwrap_err(), "write_ledger");
                refused(fetch_ledger::record_fetch(&lock, &a, "main").unwrap_err(), "record_fetch");
            }
            assert_eq!(read("fetched-heads"), None, "{:?}: the ledger is unwritten", phase);

            refused(held_heads::record_held_head("main", &a).unwrap_err(), "record_held_head");
            assert_eq!(read("held-heads"), None, "{:?}: the held heads are unwritten", phase);

            refused(sealed_utils::append(&[(SealOp::Record, &b)]).unwrap_err(), "append");
            refused(sealed_utils::compact().unwrap_err(), "compact");
            assert_eq!(std::fs::read(sealed_utils::sealed_path()).unwrap(), sealed_before, "{:?}", phase);

            std::fs::write(sealed_utils::sealed_path(), &damaged).unwrap();
            refused(sealed_utils::rebuild().unwrap_err(), "rebuild");
            assert_eq!(std::fs::read(sealed_utils::sealed_path()).unwrap(), damaged, "{:?}: not rebuilt", phase);
            std::fs::write(sealed_utils::sealed_path(), &sealed_before).unwrap();

            let lock = RefLock::acquire().unwrap();
            finish(&lock, &token).unwrap();
        }

        // With the intent gone every write lands.
        let lock = RefLock::acquire().unwrap();
        fetch_ledger::record_fetch(&lock, &a, "main").unwrap();
        drop(lock);
        assert!(read("fetched-heads").is_some());
        held_heads::record_held_head("main", &a).unwrap();
        assert!(read("held-heads").is_some());
        sealed_utils::append(&[(SealOp::Record, &b)]).unwrap();
        assert!(sealed_utils::read_sealed().unwrap().contains(&b));
        let appended = std::fs::read(sealed_utils::sealed_path()).unwrap();
        sealed_utils::compact().unwrap();
        assert_ne!(std::fs::read(sealed_utils::sealed_path()).unwrap(), appended, "the compaction rewrote it");
        std::fs::write(sealed_utils::sealed_path(), &damaged).unwrap();
        assert!(sealed_utils::rebuild().unwrap().is_some());
    }

    /// The origin is rewritten only by the move that owns the intent, once it is rewriting, and only
    /// to the target it recorded then: not in the checking phase, not with a replaced move's token,
    /// not to another server, and not with no intent at all.
    ///
    /// Revert: skip `require_rewriting` in `write_locked` → the checking-phase row rewrites the
    /// origin → red; or drop its target comparison → the other-server row rewrites it → red; or
    /// compare byte for byte → the trailing-slash row refuses → red.
    #[test]
    fn only_a_rewriting_move_rewrites_the_origin() {
        use crate::util::origin_utils;

        let _scratch = Scratch::new("rewrite-origin");
        origin_utils::write_for_tests("http://old.example");
        let origin = || origin_utils::read_origin().unwrap().unwrap();

        let checking = begin_for_tests(Phase::Checking);
        let lock = RefLock::acquire().unwrap();
        let error = origin_utils::write_locked(&lock, &checking, "http://new-origin.example").unwrap_err();
        assert!(error.contains("not started rewriting"), "{}", error);
        assert_eq!(origin(), "http://old.example");
        drop(lock);

        let rewriting = begin_for_tests(Phase::Rewriting);
        let lock = RefLock::acquire().unwrap();
        assert!(origin_utils::write_locked(&lock, &checking, "http://new.example").is_err(), "a replaced token");
        assert_eq!(origin(), "http://old.example");

        let error = origin_utils::write_locked(&lock, &rewriting, "http://unchecked.example").unwrap_err();
        assert!(error.contains("rewriting it to \"http://new-origin.example\""), "{}", error);
        assert_eq!(origin(), "http://old.example");

        origin_utils::write_locked(&lock, &rewriting, "http://new-origin.example/").unwrap();
        assert_eq!(origin(), "http://new-origin.example/", "a trailing slash names the same server");
        origin_utils::write_locked(&lock, &rewriting, "http://new-origin.example").unwrap();
        assert_eq!(origin(), "http://new-origin.example");

        finish(&lock, &rewriting).unwrap();
        assert!(origin_utils::write_locked(&lock, &rewriting, "http://new-origin.example").is_err(), "no intent");
        assert_eq!(origin(), "http://new-origin.example");
    }

    /// The funnel check passes with no intent and refuses with `origin_move_pending` in either
    /// phase, through a `String`, naming the move's target; a damaged intent refuses the same way,
    /// naming its file, never reading as no intent.
    ///
    /// Revert: read a file that does not parse as no intent → the damaged rows pass the check.
    #[test]
    fn the_funnel_check_refuses_under_any_intent_and_a_damaged_one() {
        let _scratch = Scratch::new("funnel");
        let lock = RefLock::acquire().unwrap();
        refuse_if_pending(&lock).unwrap();
        drop(lock);

        for phase in [Phase::Checking, Phase::Rewriting] {
            let token = begin_for_tests(phase);
            let lock = RefLock::acquire().unwrap();
            let error = refuse_if_pending(&lock).unwrap_err();
            assert!(error.contains(phase.as_str()) && error.contains("http://new-origin.example"), "{}", error);
            assert_eq!(refusal_code(error), Some(RefusalCode::OriginMovePending), "{:?}", phase);
            assert_eq!(ensure_none_pending().map_err(|e| e.to_string()).unwrap_err().contains("has not finished"), true);
            finish(&lock, &token).unwrap();
        }

        let path = intent_path();
        for damaged in [
            b"token ab\nphase checking\ntarget http://x\n".to_vec(),
            record_utils::to_bytes(RECORD, VERSION, ["token ab", "phase checking"]),
            record_utils::to_bytes(RECORD, VERSION, ["token zz", "phase checking", "target http://x"]),
            record_utils::to_bytes(RECORD, VERSION, ["token ab", "phase done", "target http://x"]),
            record_utils::to_bytes(RECORD, VERSION, ["token ab", "phase checking", "target "]),
        ] {
            std::fs::write(&path, &damaged).unwrap();
            let lock = RefLock::acquire().unwrap();
            let error = refuse_if_pending(&lock).unwrap_err();
            assert!(error.contains(FILE_NAME), "{}", error);
            assert_eq!(refusal_code(error), Some(RefusalCode::OriginMovePending), "a damaged intent refuses");
        }
    }
}
