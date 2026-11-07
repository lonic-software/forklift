//! Redactions (FORK-359): a permanent record that one stored object must be removed from the
//! warehouse, and never stored in it again.
//!
//! A redaction is a cell on a meta pallet (`@redactions`, on the meta-tree substrate,
//! `meta_tree`), so who redacted it is the cell's stamp: a signing key under trust, the declared
//! operator in a warehouse that never enrolled. The cell sits at `cells/<hash>`, where `<hash>` is
//! the full hash of the redacted object (a file's blob, or a chunked file's recipe), and holds a
//! `redactions.redaction 1` value: the reason, when it was recorded and, for a recipe, a digest
//! of its ordered chunk list, so the same file chunked the same way is recognised once the recipe
//! itself is gone.
//!
//! A redaction is never changed or removed, wherever the content check runs, and a signed parcel
//! that adds one must be signed by an Admin of the office as it stands when the parcel is admitted
//! — the convention `@tags` follows, for the same reasons. A hash some meta pallet's history names
//! is refused when the redaction is recorded: a meta pallet's history is never rewritten, so its
//! content could never be removed. So is one a pallet head names, until the file is taken out of
//! it; that refusal only comes early, since no head written after the redaction may name the
//! content either way.

use std::collections::{HashMap, HashSet};
use toml_edit::{value, Table};
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::object_type::ObjectType;
use crate::model::operator::Operator;
use crate::util::meta_tree::{
    self, CellEnvelope, CellRead, MetaKind, MetaWriter, PalletRules, ParcelChange,
};
use crate::util::office_utils::{self, OfficeState, Role};
use crate::util::{file_utils, object_utils, pallet_utils};

/// The name of the redactions meta pallet. Lives in the meta namespace, so it is reached as
/// `@redactions` and reserves no user pallet name (DESIGN.html §3.3).
pub const REDACTIONS_PALLET_NAME: &str = "redactions";

/// The one value schema this build writes and reads.
pub const REDACTION_SCHEMA: &str = "redactions.redaction 1";

/// The longest reason, in bytes.
pub const MAX_REASON_BYTES: usize = 1024;

const FIELD_REASON: &str = "reason";
const FIELD_REDACTED_AT: &str = "redacted_at";
const FIELD_CHUNKS: &str = "chunks";

/// One redaction: the object it removes and why. Carries no redactor field — the redactor is the
/// stamp on its cell.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Redaction {
    /// The full hash of the redacted object. Immutable once recorded.
    pub hash: String,

    /// Why it was redacted (never empty).
    pub reason: String,

    /// When it was recorded. Display metadata only — never a security input.
    pub redacted_at: i64,

    /// For a recipe, the digest of its ordered chunk list ([`chunk_list_digest`]); `None` for a
    /// blob.
    pub chunks: Option<String>,
}

/// A redaction together with its authorship: forge-proof when signed, declared when not.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttributedRedaction {
    pub redaction: Redaction,

    /// The operator whose key the cell is stamped with (the key id when the office does not know
    /// that key), or the operator an unsigned cell declares.
    pub redactor: String,

    /// The key id the cell is stamped with, or `None` when it was written without a key.
    pub signer: Option<String>,
}

/// A redaction cell this build cannot read: a newer schema, or a malformed cell. It is listed,
/// never dropped and never allowed to fail a read of the others.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnreadableRedaction {
    /// The redacted hash — the cell's path.
    pub hash: String,

    /// Why it cannot be read.
    pub reason: String,
}

/// Every redaction at one head.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RedactionsRead {
    /// Every readable redaction, by hash.
    pub redactions: Vec<AttributedRedaction>,

    /// Every redaction cell this build cannot read.
    pub unreadable: Vec<UnreadableRedaction>,
}

/// Validate a redaction's cell path: a full object hash.
pub fn validate_redacted_hash(hash: &str) -> Result<(), String> {
    match object_utils::is_object_hash(hash) {
        true => Ok(()),
        false => Err(format!("\"{}\" is not a full object hash", hash)),
    }
}

/// The digest of a recipe's ordered chunk list: the BLAKE3 hash of its chunk hashes, each
/// followed by a newline. Two recipes share it exactly when they list the same chunks in the
/// same order, which is what stays recognisable once a redacted recipe has been deleted.
pub fn chunk_list_digest(chunks: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();

    for chunk in chunks {
        hasher.update(chunk.as_bytes());
        hasher.update(b"\n");
    }

    hasher.finalize().to_hex().to_string()
}

fn validate_reason(reason: &str) -> Result<(), String> {
    if reason.trim().is_empty() {
        return Err("has an empty reason".to_string());
    }

    if reason.len() > MAX_REASON_BYTES {
        return Err(format!(
            "has a reason of {} bytes; at most {} are allowed",
            reason.len(), MAX_REASON_BYTES
        ));
    }

    Ok(())
}

/// A `redactions.redaction 1` value table.
fn redaction_value(redaction: &Redaction) -> Table {
    let mut table = Table::new();
    table.insert(FIELD_REASON, value(redaction.reason.as_str()));
    table.insert(FIELD_REDACTED_AT, value(redaction.redacted_at));

    if let Some(chunks) = &redaction.chunks {
        table.insert(FIELD_CHUNKS, value(chunks.as_str()));
    }

    table
}

/// Parse a `redactions.redaction 1` value table, strictly: only its fields, a non-empty reason,
/// and a chunk-list digest, when present, that is a full hash.
fn parse_value(hash: &str, table: &Table) -> Result<Redaction, String> {
    meta_tree::value_fields_only(table, &[FIELD_REASON, FIELD_REDACTED_AT, FIELD_CHUNKS])?;

    let reason = meta_tree::value_string(table, FIELD_REASON)?;
    validate_reason(&reason)?;

    let chunks = match table.contains_key(FIELD_CHUNKS) {
        true => Some(meta_tree::value_string(table, FIELD_CHUNKS)?),
        false => None,
    };

    if chunks.as_deref().is_some_and(|chunks| !object_utils::is_object_hash(chunks)) {
        return Err("has a chunk-list digest that is not a full hash".to_string());
    }

    Ok(Redaction {
        hash: hash.to_string(),
        reason,
        redacted_at: meta_tree::value_integer(table, FIELD_REDACTED_AT)?,
        chunks,
    })
}

/// The pallet rules of `@redactions`: one cell per full object hash, a strict value, an Admin to
/// add one (checked when a parcel is admitted, not when admitted history is audited), and
/// immutability.
pub struct RedactionsRules;

impl PalletRules for RedactionsRules {
    fn kind(&self) -> MetaKind {
        MetaKind::Redactions
    }

    fn knows_schema(&self, schema: &str) -> bool {
        schema == REDACTION_SCHEMA
    }

    fn check(&self, change: &ParcelChange<'_>) -> Result<(), String> {
        for cell in change.changes {
            validate_redacted_hash(&cell.path)?;

            if cell.before.is_some() {
                return Err(format!(
                    "the redaction of {} already exists, and a redaction is never changed or \
                    removed",
                    cell.path
                ));
            }

            let Some(after) = &cell.after else { continue };

            if let Some((office, operator)) = change.role_check() {
                let is_admin = office.find_user(operator)
                    .is_some_and(|user| matches!(user.role, Role::Admin));

                if !is_admin {
                    return Err(format!(
                        "the redaction of {} is signed by {}, who is not an admin",
                        cell.path, operator
                    ));
                }
            }

            let envelope = meta_tree::parse_cell(&object_utils::load_blob(after)?.content)?;

            if envelope.schema == REDACTION_SCHEMA {
                parse_value(&cell.path, &envelope.value)
                    .map_err(|why| format!("the redaction of {} {}", cell.path, why))?;
            }
        }

        Ok(())
    }

    fn resolution_path(&self, spelled: &str) -> Result<String, String> {
        validate_redacted_hash(spelled)?;

        Ok(spelled.to_string())
    }

    fn display_value(&self, envelope: &CellEnvelope) -> String {
        match meta_tree::value_string(&envelope.value, FIELD_REASON) {
            Ok(reason) => format!("redacted: {}", reason),
            Err(why) => format!("(unreadable: the value {})", why),
        }
    }
}

/// What a redaction removes, as the record step resolved it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RedactedKind {
    /// A file's content blob.
    Blob,

    /// A chunked file's recipe, with every chunk it names.
    Recipe,
}

/// Decide what `hash` is from the object in the local store. A tree, a parcel or a chunk is
/// refused: a redaction removes one file's content.
///
/// # Returns
/// * `Ok(Some(kind))` - The object is held, and is a blob or a recipe.
/// * `Ok(None)`       - The object is not in the local store.
/// * `Err(String)`    - The object is some other type, or could not be read.
pub fn held_kind(hash: &str) -> Result<Option<RedactedKind>, String> {
    if !file_utils::does_object_exist(hash)? {
        return Ok(None);
    }

    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
    let (object_type, _) = object_utils::peek_object_header(&bytes)
        .map_err(|e| format!("Object {} cannot be read: {:?}", hash, e))?;

    match object_type {
        ObjectType::Blob => Ok(Some(RedactedKind::Blob)),
        ObjectType::Recipe => Ok(Some(RedactedKind::Recipe)),
        other => Err(format!(
            "Object {} is a {:?}, not a file's content; name the file to redact as \
            <revision>:<path>.",
            hash, other
        )),
    }
}

/// The kind a tree entry's content is redacted as, or `None` for an entry with no content of its
/// own (a subtree).
pub fn entry_kind(entry_type: DirEntryType) -> Option<RedactedKind> {
    match entry_type {
        DirEntryType::Tree => None,
        _ if entry_type.is_chunked() => Some(RedactedKind::Recipe),
        _ => Some(RedactedKind::Blob),
    }
}

/// Every meta pallet whose history names `hash` anywhere in a parcel's tree, by name. Walks every
/// meta pallet's whole chain, each parcel and tree once per pallet. An object the walk needs and
/// the store lacks is an error, never a pass: the walk could not tell whether it names `hash`.
/// A pallet that does name it is reported ahead of that error, as the more definite answer.
pub fn meta_holders(hash: &str) -> Result<Vec<String>, String> {
    let mut holders = Vec::new();
    let mut undecided = None;

    for name in pallet_utils::list_meta_pallets()? {
        let Some(head) = pallet_utils::get_meta_pallet_head(&name)? else { continue };

        match chain_names(&name, &head, hash) {
            Ok(true) => holders.push(name),
            Ok(false) => {}
            Err(error) => { undecided.get_or_insert(error); }
        }
    }

    match undecided {
        Some(error) if holders.is_empty() => Err(error),
        _ => Ok(holders),
    }
}

/// Whether any parcel reachable from `head`, the head of meta pallet `pallet`, has a tree naming
/// `hash` at any depth.
pub(crate) fn chain_names(pallet: &str, head: &str, hash: &str) -> Result<bool, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending = vec![(head.to_string(), true)];

    // Each entry is an object hash and whether it is a parcel (else a tree).
    while let Some((object, is_parcel)) = pending.pop() {
        if object == hash {
            return Ok(true);
        }

        if !seen.insert(object.clone()) {
            continue;
        }

        // A meta chain is checked from its root wherever it is adopted, so an object missing
        // here is damage, not a partial fetch; the walk fails rather than pass a hash it could
        // not judge.
        let missing = |error: String| match file_utils::does_object_exist(&object) {
            Ok(false) => format!(
                "The store is damaged: object {} of @{}'s history is missing, so whether that \
                history names {} cannot be told. If this warehouse has a remote, a fresh clone \
                of it holds that history; otherwise the missing history cannot be recovered here.",
                object, pallet, hash
            ),
            _ => error,
        };

        if is_parcel {
            let parcel = object_utils::load_parcel(&object).map_err(missing)?;
            pending.extend(parcel.parents.into_iter().map(|parent| (parent, true)));
            pending.push((parcel.tree_hash, false));
            continue;
        }

        let tree = object_utils::load_tree_shared(&object).map_err(missing)?;

        if tree.get_files().any(|(_, file)| file.hash == hash) {
            return Ok(true);
        }

        pending.extend(tree.get_subtrees().map(|(_, subtree)| (subtree.hash.clone(), false)));
    }

    Ok(false)
}

/// A user pallet head that names the content a redaction would remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadHolder {
    /// The pallet.
    pub pallet: String,

    /// Its head.
    pub head: String,

    /// Where the head's tree holds the content.
    pub path: String,

    /// The hash at that path: the redacted hash, or for a recipe, another with the same chunk
    /// list.
    pub hash: String,
}

/// Every user pallet whose head names what `redaction` removes, by the rule a head write is
/// judged by: the redacted hash anywhere in the head's tree, or for a recipe, a chunked file with
/// the same chunk list. A partial warehouse skips a head, subtree or recipe it does not hold, as
/// the write check does, so a head can name the content in a part this warehouse never fetched.
/// Elsewhere an object the walk needs and the store lacks is an error, never a pass; a pallet that
/// does name the content is reported ahead of that error.
pub fn head_holders(redaction: &Redaction) -> Result<Vec<HeadHolder>, String> {
    let set = RedactedSet {
        hashes: HashSet::from([redaction.hash.clone()]),
        chunk_lists: redaction.chunks.iter().map(|chunks| (chunks.clone(), HashSet::from([redaction.hash.clone()]))).collect(),
    };

    let partial = crate::util::scope_utils::content_may_be_absent()?;
    let mut holders = Vec::new();
    let mut undecided = None;

    for pallet in pallet_utils::list_pallets()? {
        let Some(head) = pallet_utils::get_pallet_head(&pallet)? else { continue };

        if partial && !file_utils::does_object_exist(&head)? {
            continue;
        }

        let mut skipped = HashSet::new();
        let named = object_utils::load_parcel(&head)
            .and_then(|parcel| tree_names(&parcel.tree_hash, &set, partial.then_some(&mut skipped)));

        match named {
            Ok(Some((path, hash))) => holders.push(HeadHolder { pallet, head, path, hash }),
            Ok(None) => {}
            Err(error) => {
                undecided.get_or_insert(format!(
                    "The head of pallet \"{}\" ({}) cannot be read ({}), so whether it names {} cannot \
                    be told.",
                    pallet, head, error.trim_end_matches('.'), redaction.hash
                ));
            }
        }
    }

    match undecided {
        Some(error) if holders.is_empty() => Err(error),
        _ => Ok(holders),
    }
}

/// The command that records the redaction of the file at `path` in parcel `head`, for a next step
/// given once that head is no longer a pallet's: `<revision>:<path>` resolves whether or not the
/// store holds the content, where a bare hash needs it held.
pub fn record_command(head: &str, path: &str) -> String {
    record_command_when(head, path, cfg!(windows))
}

/// [`record_command`], quoting for Windows when `windows`.
fn record_command_when(head: &str, path: &str, windows: bool) -> String {
    // The path is quoted on its own; behind the head it never starts the argument, so it needs no
    // `--` either.
    match crate::util::tree_utils::remedy_path_when(path, windows) {
        Some(quoted) => format!(
            "forklift redact record {}:{} -m <reason>",
            head, quoted.strip_prefix("-- ").unwrap_or(&quoted)
        ),
        None => format!(
            "forklift redact record {}:<path> -m <reason>, with that path quoted for your shell",
            head
        ),
    }
}

/// Record a redaction, as one cell on the local `@redactions` head stamped by `writer`. The
/// caller holds the warehouse lock, and has resolved the hash and checked it against the meta
/// pallets and the pallet heads; the content check refuses a signer who is not an admin, and a
/// hash already redacted (a fresh `redacted_at` always changes the cells, so a repeat reaches that
/// check).
pub fn record_redaction(redaction: &Redaction,
                        writer: &MetaWriter,
                        actor: &Operator) -> Result<String, String> {
    validate_redacted_hash(&redaction.hash)?;
    validate_reason(&redaction.reason).map_err(|why| format!("The redaction {}.", why))?;

    let stamp = writer.stamp(actor);
    let value = redaction_value(redaction);
    let edits = [meta_tree::set_cell(&redaction.hash, &stamp, REDACTION_SCHEMA, &value)?];
    let description = format!("redact {}", redaction.hash);

    let rules = &RedactionsRules;
    meta_tree::stack_meta_edits(REDACTIONS_PALLET_NAME, rules, writer, &edits, actor, &description)?
        .ok_or(format!(
            "The redaction of {} changed nothing, so nothing was recorded.",
            redaction.hash
        ))
}

/// Read every redaction at the local head, attributed through the office (read best-effort, so
/// one whose key the local office does not know is attributed by key id) or by the operator an
/// unsigned cell declares, with every cell this build cannot read. An unborn pallet reads as
/// empty.
pub fn read_redactions() -> Result<RedactionsRead, String> {
    let Some(state) = meta_tree::read_meta_head(REDACTIONS_PALLET_NAME, &RedactionsRules)? else {
        return Ok(RedactionsRead::default());
    };

    let office = office_utils::read_office_state()
        .unwrap_or(OfficeState { users: Vec::new(), keys: Vec::new() });

    let mut read = RedactionsRead::default();

    for (hash, cell) in state.cells {
        let parsed = match cell {
            CellRead::Readable(envelope) => parse_value(&hash, &envelope.value)
                .map(|redaction| (redaction, envelope.stamp))
                .map_err(|why| format!("the value {}", why)),
            CellRead::Unknown { schema, .. } => Err(format!(
                "written with the schema \"{}\", which this forklift does not know",
                schema
            )),
            CellRead::Malformed(why) => Err(why),
        };

        match parsed {
            Ok((redaction, stamp)) => read.redactions.push(AttributedRedaction {
                redaction,
                redactor: stamp.operator_in(&office),
                signer: stamp.key_id().map(str::to_string),
            }),
            Err(reason) => read.unreadable.push(UnreadableRedaction { hash, reason }),
        }
    }

    Ok(read)
}

/// What the redactions at one `@redactions` head forbid a new head to name: every redacted hash,
/// readable or not (a cell this build cannot read still names its hash, and still refuses it),
/// and the chunk-list digest of every redacted recipe, with the redacted recipes whose list it is.
#[derive(Debug, Default)]
struct RedactedSet {
    hashes: HashSet<String>,
    chunk_lists: HashMap<String, HashSet<String>>,
}

impl RedactedSet {
    /// The set at `head`, the head of `@redactions`.
    fn at(head: &str) -> Result<RedactedSet, String> {
        let state = meta_tree::read_meta_state(REDACTIONS_PALLET_NAME, head, &RedactionsRules)?;
        let mut set = RedactedSet::default();

        // A cell this build cannot read, or whose value does not parse, still redacts its hash; what
        // it says about a chunk list is lost with it, so the same chunks under another recipe are
        // not caught until the cell is read by a forklift that understands it.
        for (hash, cell) in state.cells {
            if let CellRead::Readable(envelope) = &cell {
                if let Ok(Redaction { chunks: Some(chunks), .. }) = parse_value(&hash, &envelope.value) {
                    set.chunk_lists.entry(chunks).or_default().insert(hash.clone());
                }
            }

            set.hashes.insert(hash);
        }

        Ok(set)
    }

    /// This set less every redaction `other` holds: its hashes, and their claim on each chunk list,
    /// so a list another recipe here shares is still held. A redaction `other` holds by hash goes
    /// whole, whatever `other` read of its chunk list.
    fn without(mut self, other: &RedactedSet) -> RedactedSet {
        self.hashes.retain(|hash| !other.hashes.contains(hash));
        self.chunk_lists.retain(|_, owners| {
            owners.retain(|owner| !other.hashes.contains(owner));
            !owners.is_empty()
        });
        self
    }

    /// The part of this set whose redactions `keep` says to keep: their hashes, and the chunk
    /// lists of the recipes among them.
    fn filtered(&self, keep: impl Fn(&str) -> bool) -> RedactedSet {
        let mut chunk_lists = self.chunk_lists.clone();
        chunk_lists.retain(|_, owners| {
            owners.retain(|owner| keep(owner));
            !owners.is_empty()
        });

        RedactedSet { hashes: self.hashes.iter().filter(|hash| keep(hash)).cloned().collect(), chunk_lists }
    }

    /// Whether this set names nothing.
    fn is_empty(&self) -> bool {
        self.hashes.is_empty() && self.chunk_lists.is_empty()
    }

    /// Whether this set names `hash`, a file a walk found redacted: the hash itself, or a recipe
    /// whose chunk list is one this set holds. Only ever asked of a file the walk could read, so a
    /// recipe that cannot be read is not one this set names.
    fn names_hash(&self, hash: &str) -> bool {
        if self.hashes.contains(hash) {
            return true;
        }

        !self.chunk_lists.is_empty() && object_utils::recipe_chunk_hashes(hash)
            .is_ok_and(|chunks| self.chunk_lists.contains_key(&chunk_list_digest(&chunks)))
    }

    /// The redacted hash a file entry names: its own hash, or for a chunked file whose recipe the
    /// store holds, the hash of the redaction its chunk list matches. `partial` tolerates a recipe
    /// the store lacks, which then carries no chunks this warehouse could hand on, and records it
    /// as skipped.
    fn named_by(&self,
                entry: &crate::model::tree_item::TreeItem,
                partial: Option<&mut HashSet<String>>) -> Result<Option<String>, String> {
        self.names_file(&entry.hash, entry.item_type, partial)
    }

    /// [`RedactedSet::named_by`] for a file of content `hash` stored as `item_type`.
    fn names_file(&self,
                  hash: &str,
                  item_type: DirEntryType,
                  partial: Option<&mut HashSet<String>>) -> Result<Option<String>, String> {
        if self.hashes.contains(hash) {
            return Ok(Some(hash.to_string()));
        }

        if self.chunk_lists.is_empty() || !item_type.is_chunked() {
            return Ok(None);
        }

        let chunks = match object_utils::recipe_chunk_hashes(hash) {
            Ok(chunks) => chunks,
            Err(error) => match partial {
                Some(skipped) if !file_utils::does_object_exist(hash)? => {
                    skipped.insert(hash.to_string());
                    return Ok(None);
                }
                _ => return Err(error),
            },
        };

        Ok(self.chunk_lists.contains_key(&chunk_list_digest(&chunks)).then(|| hash.to_string()))
    }
}

/// A redacted file a tree names: its path from the tree's root, and the redacted hash.
type Named = (String, String);

/// The first file under `tree` that names content in `set`, walking every subtree once. `partial`
/// (a sparse or cache warehouse) skips a subtree or recipe the store does not hold: its bytes are
/// not here, so this warehouse cannot hand them on. A head that receives the parcel judges it
/// against its own redactions, if it checks at all (the AWS head does not yet), so a redaction
/// recorded only here does not follow a skipped subtree. Each skipped object is recorded in
/// `partial`, so the write's hold can see one stored meanwhile. In a full warehouse a missing
/// object is an error, never a pass.
fn tree_names(tree: &str, set: &RedactedSet, mut partial: Option<&mut HashSet<String>>) -> Result<Option<Named>, String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending = vec![(String::new(), tree.to_string())];

    while let Some((prefix, hash)) = pending.pop() {
        if !seen.insert(hash.clone()) {
            continue;
        }

        let loaded = match object_utils::load_tree_shared(&hash) {
            Ok(loaded) => loaded,
            Err(error) => match partial.as_deref_mut() {
                Some(skipped) if !file_utils::does_object_exist(&hash)? => {
                    skipped.insert(hash);
                    continue;
                }
                _ => return Err(error),
            },
        };

        for (name, file) in loaded.get_files() {
            if let Some(redacted) = set.named_by(file, partial.as_deref_mut())? {
                return Ok(Some((format!("{}{}", prefix, name), redacted)));
            }
        }

        pending.extend(loaded.get_subtrees()
            .map(|(name, subtree)| (format!("{}{}/", prefix, name), subtree.hash.clone())));
    }

    Ok(None)
}

/// The redactions a bay-local write judges what it stages against: `load`, `park`, and a merge or
/// pick before it touches the working directory. Bay-local state is no pallet head, so nothing
/// requires this; it refuses early what the next head write would refuse, before the content is
/// staged again or written back into the bay. Judged by the head-write rule: the redacted hash, or
/// for a recipe, a chunked file with the same chunk list. A partial warehouse tolerates a subtree
/// or recipe it does not hold, as the head write does.
pub struct StagingGuard {
    set: RedactedSet,
    partial: bool,

    /// The `@redactions` head `set` was read at.
    head: String,
}

/// The guard for the redactions at the current `@redactions` head, or `None` while there are
/// none, so a warehouse without redactions reads nothing more.
pub fn staging_guard() -> Result<Option<StagingGuard>, String> {
    let Some(head) = pallet_utils::get_meta_pallet_head(REDACTIONS_PALLET_NAME)? else {
        return Ok(None);
    };

    let set = RedactedSet::at(&head)?;

    if set.hashes.is_empty() {
        return Ok(None);
    }

    let partial = crate::util::scope_utils::content_may_be_absent()?;

    Ok(Some(StagingGuard { set, partial, head }))
}

impl StagingGuard {
    /// The redacted hash a file of content `hash`, stored as `item_type`, names, if any.
    pub fn names_file(&self, hash: &str, item_type: DirEntryType) -> Result<Option<String>, String> {
        let mut skipped = HashSet::new();

        self.set.names_file(hash, item_type, self.partial.then_some(&mut skipped))
    }

    /// The first file under `tree` naming redacted content, as its path from the tree's root and
    /// the redacted hash.
    pub fn names_tree(&self, tree: &str) -> Result<Option<(String, String)>, String> {
        let mut skipped = HashSet::new();

        tree_names(tree, &self.set, self.partial.then_some(&mut skipped))
    }
}

/// Refuse checking out a tree whose `ops`, what it writes into the bay, would bring back content a
/// redaction removed, before the bay is touched: the head write it leads to would be refused too,
/// but only once the content had reached the bay. Judged after the presence phase, which makes
/// every recipe they write readable. `adopting` is what the remote served when the checkout takes
/// its head as it is, which the head write then adopts: the redactions it holds pending are not
/// judged here either, while this root has never been served (see [`check_head_write`]). `doing`
/// names the operation; `next_step` is given the path.
pub fn refuse_redacted_writes(ops: &[crate::util::shift_utils::FileOp],
                              adopting: Option<Served>,
                              doing: &str,
                              next_step: impl Fn(&str) -> String) -> Result<(), String> {
    let Some(mut guard) = staging_guard()? else {
        return Ok(());
    };

    // What the adoption would have exempted but for this warehouse having applied it.
    let mut applied = RedactedSet::default();

    if let Some(served) = adopting {
        let applied_here = applied_part(&guard.set)?;
        let exemption = exempt_adoption(std::mem::take(&mut guard.set), Some(served), served.head, &guard.head, &applied_here)?;
        guard.set = exemption.set;
        applied = exemption.withheld;
    }

    for op in ops {
        let crate::util::shift_utils::FileOp::Write { path, hash, item_type, .. } = op else {
            continue;
        };

        if let Some(hash) = guard.names_file(hash, *item_type)? {
            let next_step = match applied.names_hash(&hash) {
                true => fresh_clone_next_step(),
                false => next_step(path),
            };

            return Err(staging_refusal(doing, path, &hash, "Nothing was changed.", next_step));
        }
    }

    Ok(())
}

/// The `content_redacted` refusal of a bay-local write that would stage the file at `path`, whose
/// content `hash` is redacted: "Refusing to `doing`", then `outcome`, which says what was left as it
/// was. `details` carries the path and the hash; no pallet head was refused, so it names none.
pub fn staging_refusal(doing: &str, path: &str, hash: &str, outcome: &str, next_step: String) -> String {
    crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentRedacted,
        format!(
            "Refusing to {}: \"{}\" holds content ({}) that is redacted in this warehouse, and \
            nothing may bring it back. {}",
            doing, path, hash, outcome
        ),
        next_step,
        serde_json::json!({ "path": path, "hash": hash }),
    ).into()
}

/// The proof a ref write's new head was checked against the redactions: which write, the
/// `@redactions` head it was checked against, and what a partial warehouse's walk skipped as
/// absent. Built only by [`check_head_write`], so a write that holds one ran the check; the
/// compare-and-set takes it and, under the ref lock, refuses it when it is for another write, or
/// `@redactions` has moved or a skipped object has been stored since (see
/// [`RedactionCheck::holds`]).
#[derive(Debug)]
pub struct RedactionCheck {
    namespace: pallet_utils::PalletNamespace,
    name: String,
    expected: Option<String>,
    new: String,
    redactions: Option<String>,

    /// What a partial warehouse's walk skipped as absent.
    skipped: HashSet<String>,

    /// Whether the check exempted what the remote holds pending, as an adoption into a root never
    /// served (see [`check_head_write`]).
    adopted: bool,

    /// The redactions an exemption passed while this warehouse had not applied them. One applied
    /// since is never exempt, so the hold checks again.
    exempted: HashSet<String>,
}

impl RedactionCheck {
    /// Whether this check is for the write of `new` over `expected` to pallet `name` in
    /// `namespace`.
    pub(crate) fn is_for(&self,
                         namespace: pallet_utils::PalletNamespace,
                         name: &str,
                         expected: Option<&str>,
                         new: &str) -> bool {
        self.namespace == namespace && self.name == name && self.expected.as_deref() == expected
            && self.new == new
    }

    /// Whether the check still holds: `@redactions` is at the head it was checked against,
    /// nothing the walk skipped as absent is stored now, an adoption's root is still not served,
    /// and no redaction an exemption passed has been applied since. Read under the held ref lock,
    /// which every `@redactions` write and every write of the applied record takes too, so a
    /// redaction either lands, or applies, before the write's hold or after the write. Storing an
    /// object takes no ref lock, so a skipped object stored after this read can still land beside
    /// the written head, as one fetched after the write can; the receiving side of a lift, not this
    /// check, is what judges bytes handed on.
    pub(crate) fn holds(&self, _lock: &crate::util::lock_utils::RefLock) -> Result<bool, String> {
        if pallet_utils::get_meta_pallet_head(REDACTIONS_PALLET_NAME)? != self.redactions {
            return Ok(false);
        }

        // The served mark is written under the ref lock too, so a root first served meanwhile
        // sends the write round again, and it is checked in full.
        if self.adopted && crate::util::tombstone_utils::is_served()? {
            return Ok(false);
        }

        // The applied record grows under the ref lock too.
        if !self.exempted.is_empty() {
            let applied = crate::util::applied_redactions::read_applied()?;

            if self.exempted.iter().any(|hash| applied.contains(hash)) {
                return Ok(false);
            }
        }

        for hash in &self.skipped {
            if file_utils::does_object_exist(hash)? {
                return Ok(false);
            }
        }

        Ok(true)
    }
}

/// What [`check_head_write`] decided, and what it read deciding it, so the write's hold can tell
/// whether the decision still stands.
#[derive(Debug)]
pub struct HeadWriteCheck {
    /// The `@redactions` head read before the check, or `None` when it could not be read.
    redactions: Option<Option<String>>,
    outcome: Result<RedactionCheck, pallet_utils::RefUpdateError>,
}

impl HeadWriteCheck {
    /// Whether the decision still stands under the held ref lock; when it does not, the write
    /// checks again. A pass stands while [`RedactionCheck`]'s inputs are unchanged. A refusal
    /// always stands: redactions are only ever added, and the head's tree does not change. A
    /// check that failed stands until `@redactions` moves, so it is retried once a redaction
    /// lands rather than reporting a failure about a head that no longer holds; one that could
    /// not read `@redactions` at all stands, and its failure is reported.
    pub(crate) fn stands(&self, lock: &crate::util::lock_utils::RefLock) -> Result<bool, String> {
        match &self.outcome {
            Ok(check) => check.holds(lock),
            Err(pallet_utils::RefUpdateError::Redacted { .. }) => Ok(true),
            Err(_) => match &self.redactions {
                Some(read) => Ok(*read == pallet_utils::get_meta_pallet_head(REDACTIONS_PALLET_NAME)?),
                None => Ok(true),
            },
        }
    }

    /// The decision: the proof the compare-and-set takes, or the refusal or failure.
    pub(crate) fn outcome(&self) -> Result<&RedactionCheck, pallet_utils::RefUpdateError> {
        self.outcome.as_ref().map_err(Clone::clone)
    }
}

/// What the remote served in the same command for the pallet a client writes: the head it
/// served, and the `@redactions` head it served beside it. Never for a write a remote sends this
/// warehouse.
#[derive(Clone, Copy, Debug)]
pub struct Served<'a> {
    /// The head the remote served for this pallet.
    pub head: &'a str,

    /// The `@redactions` head the remote served in the same handshake, `None` when it served
    /// none. What it holds is what the remote reports pending; read for a user pallet only.
    pub redactions: Option<&'a str>,
}

impl<'a> Served<'a> {
    /// A meta pallet's served `head`: its chain is what the write takes.
    pub fn chain(head: &'a str) -> Served<'a> {
        Served { head, redactions: None }
    }

    /// A user pallet's served `head`, beside the `@redactions` head `info`, the remote's
    /// handshake, serves.
    pub fn adopting(head: &'a str, info: &'a crate::model::remote::WarehouseInfo) -> Served<'a> {
        let redactions = info.pallets.get(&pallet_utils::PalletRef::meta(REDACTIONS_PALLET_NAME).to_wire());

        Served { head, redactions: redactions.map(String::as_str) }
    }
}

/// Check a ref write of `new` over `expected` against the redactions at the current `@redactions`
/// head, and return the proof the compare-and-set takes. Run before the ref lock, since it walks;
/// a redaction landing between this and the hold sends the write round again (see
/// [`HeadWriteCheck::stands`]).
///
/// - A user pallet's new head is refused when its tree names a redacted file anywhere. The whole
///   tree is walked, never only what differs from `expected`: a stale branch revived onto a ref
///   must not bring the content back. The one exemption is an adoption, under `served` below.
/// - A meta pallet's write is judged by the history it adds. Each meta parcel is the pallet's
///   whole state, so a parcel that carries a cell its parent already names passes, and only one
///   that adds a redacted object to the tree is refused; refusing the carry would freeze a pallet
///   whose history named the object before it was redacted.
/// - `served`, for a meta pallet, exempts the history its head reaches:
///   the remote judged each of those parcels against its own redactions when it took it, which
///   need not hold this warehouse's, but whatever they name is held there by meta history that is
///   never rewritten, so refusing would only freeze the pallet behind it. A client adopting the remote's
///   chain (a lower's fast-forward, the trust install's office, a replay onto the remote's head) is
///   then not refused for history it merely takes. A parcel past `served` is judged as usual. It
///   is `None` for every write a remote sends this warehouse.
/// - Neither exemption covers a redaction this warehouse has applied: the served chain's parcels
///   are judged against those, and an adoption is refused for one, naming a fresh clone as the way
///   out (see [`fresh_clone_next_step`]).
/// - `served`, for a user pallet, exempts one write: adopting exactly the head the remote served
///   (a lower or clone taking it), into a root that has never been served. It exempts only the
///   redactions the remote reports pending, those at the `@redactions` head it served, and only
///   once this warehouse's own `@redactions` reaches that head, so it was verified and taken here;
///   one recorded only here still refuses. The remote's heads naming that content can only shrink, so
///   adopting one adds none that anyone is served, and the owner can still fetch the pallet they
///   must fix. In a root a forklift server has served the write is checked like any other, and
///   the hold re-reads the served mark. A repair, a merge or any other new parcel is checked as
///   usual.
/// - A write that leaves the ref where it is, or one made while no redaction exists, walks
///   nothing.
///
/// # Returns
/// The decision and what it read. Its outcome is:
/// * `Ok(check)`                       - Nothing the write adds names redacted content.
/// * `Err(RefUpdateError::Redacted)`   - It does; nothing was written.
/// * `Err(RefUpdateError::Failed)`     - `@redactions` or an object the check needs could not be
///                                       read.
pub fn check_head_write(namespace: pallet_utils::PalletNamespace,
                        name: &str,
                        expected: Option<&str>,
                        new: &str,
                        served: Option<Served>) -> HeadWriteCheck {
    let redactions = pallet_utils::get_meta_pallet_head(REDACTIONS_PALLET_NAME);
    let outcome = match &redactions {
        Ok(redactions) => check_head_write_at(namespace, name, expected, new, served, redactions.clone()),
        Err(error) => Err(error.clone().into()),
    };

    HeadWriteCheck { redactions: redactions.ok(), outcome }
}

/// [`check_head_write`]'s decision against the redactions at `redactions`.
fn check_head_write_at(namespace: pallet_utils::PalletNamespace,
                       name: &str,
                       expected: Option<&str>,
                       new: &str,
                       served: Option<Served>,
                       redactions: Option<String>) -> Result<RedactionCheck, pallet_utils::RefUpdateError> {
    let mut check = RedactionCheck {
        namespace,
        name: name.to_string(),
        expected: expected.map(str::to_string),
        new: new.to_string(),
        redactions: redactions.clone(),
        skipped: HashSet::new(),
        adopted: false,
        exempted: HashSet::new(),
    };

    let Some(redactions) = redactions.filter(|_| expected != Some(new)) else {
        return Ok(check);
    };

    let mut set = RedactedSet::at(&redactions)?;

    // What this warehouse has applied no exemption covers: only an exempt write reads it.
    let applied = match served {
        Some(_) => applied_part(&set)?,
        None => RedactedSet::default(),
    };

    // What the adoption would have exempted but for this warehouse having applied it.
    let mut withheld = RedactedSet::default();

    if namespace == pallet_utils::PalletNamespace::User {
        let exemption = exempt_adoption(set, served, new, &redactions, &applied)?;
        (set, check.adopted, check.exempted, withheld) = (exemption.set, exemption.adopted, exemption.exempted, exemption.withheld);
    } else if served.is_some() {
        check.exempted = set.hashes.iter().filter(|hash| !applied.hashes.contains(*hash)).cloned().collect();
    }

    // An adoption's check is still held to the served mark, though nothing may be left to walk.
    if set.is_empty() {
        return Ok(check);
    }

    // `applied` says whether the refusal is one only because the redaction is applied here: the
    // file came from what the remote served, which the exemption would have passed otherwise.
    let (named, applied) = match namespace {
        pallet_utils::PalletNamespace::User => {
            let partial = crate::util::scope_utils::content_may_be_absent()?;
            let tree = object_utils::load_parcel(new)?.tree_hash;
            let named = tree_names(&tree, &set, partial.then_some(&mut check.skipped))?;
            let applied = named.as_ref().is_some_and(|(_, hash)| withheld.names_hash(hash));

            (named, applied)
        }
        pallet_utils::PalletNamespace::Meta => match meta_parcels_add(expected, served.map(|served| served.head), new, &set, &applied)? {
            Some(added) => (Some(added.named), added.served),
            None => (None, false),
        },
    };

    match named {
        None => Ok(check),
        Some((path, hash)) => Err(pallet_utils::RefUpdateError::Redacted {
            pallet: pallet_utils::PalletRef { namespace, name: name.to_string() },
            head: new.to_string(),
            applied,
            path,
            hash,
        }),
    }
}

/// The part of `set` this warehouse has applied, which no exemption covers: an applied hash is one
/// no head or meta history here names, and taking one that does would undo that. It arises only
/// where a warehouse applied a redaction by its own scan while it had no remote, and the remote it
/// gained since still serves history naming the hash.
fn applied_part(set: &RedactedSet) -> Result<RedactedSet, String> {
    let applied = crate::util::applied_redactions::read_applied()?;

    Ok(set.filtered(|hash| applied.contains(hash)))
}

/// The way out of a write refused for history naming a hash this warehouse applied: the remote
/// still serves that history, and nothing here can take it again, but a fresh clone takes it, and
/// the remote's redactions, as the remote holds them.
pub(crate) fn fresh_clone_next_step() -> String {
    "this warehouse has applied that redaction, so it can never take history naming the content \
    again; make a fresh clone of the remote with \"forklift clone\", which takes the remote's history \
    and redactions as the remote holds them, and work there".to_string()
}

/// What [`exempt_adoption`] decided.
struct Exemption {
    /// The redactions the write is judged by.
    set: RedactedSet,

    /// Whether the write is an adoption.
    adopted: bool,

    /// The redactions it is exempt from.
    exempted: HashSet<String>,

    /// The redactions the remote holds pending that it would be exempt from but for this warehouse
    /// having applied them.
    withheld: RedactedSet,
}

/// `set`, the redactions at this warehouse's `@redactions` head `local`, less those a user
/// pallet's write of `new` is exempt from, and whether it is an adoption. It is one when `new` is
/// the head the remote served beside `served`, and this root has never been served; it is then
/// exempt from the redactions at the `@redactions` head the remote served, which the remote
/// reports pending. Only once `local` reaches that head: this warehouse verified and took it, so a
/// set the remote merely sent, or one it never served, exempts nothing. Nor is any redaction in
/// `applied`, those this warehouse has applied, exempt.
fn exempt_adoption(set: RedactedSet,
                   served: Option<Served>,
                   new: &str,
                   local: &str,
                   applied: &RedactedSet) -> Result<Exemption, String> {
    let none = |set| Ok(Exemption { set, adopted: false, exempted: HashSet::new(), withheld: RedactedSet::default() });

    let Some(Served { head, redactions: Some(pending) }) = served else {
        return none(set);
    };

    if head != new || crate::util::tombstone_utils::is_served()? {
        return none(set);
    }

    if pending != local && !crate::util::merge_utils::is_ancestor(pending, local)? {
        return none(set);
    }

    let pending = RedactedSet::at(pending)?;
    let exempt = pending.filtered(|hash| !applied.hashes.contains(hash));
    let withheld = pending.filtered(|hash| applied.hashes.contains(hash));
    let exempted = exempt.hashes.clone();

    Ok(Exemption { set: set.without(&exempt), adopted: true, exempted, withheld })
}

/// What [`meta_parcels_add`] found.
struct Added {
    /// The redacted file, and its hash.
    named: Named,

    /// Whether a parcel of the served chain added it.
    served: bool,
}

/// The first redacted object a parcel of the meta chain at `new` adds over its parents, among the
/// parcels `expected` does not reach. One `served` reaches is judged only by `applied`, the
/// redactions this warehouse has applied, which no exemption covers. A meta chain is complete
/// wherever it is adopted, so an object missing here is an error.
fn meta_parcels_add(expected: Option<&str>,
                    served: Option<&str>,
                    new: &str,
                    set: &RedactedSet,
                    applied: &RedactedSet) -> Result<Option<Added>, String> {
    // The parcels `from` reaches that `stop` does not.
    let ancestry = |from: Option<&str>, stop: &HashSet<String>| -> Result<HashSet<String>, String> {
        let mut reached: HashSet<String> = HashSet::new();
        let mut pending: Vec<String> = from.into_iter().map(str::to_string).collect();

        while let Some(parcel) = pending.pop() {
            if !stop.contains(&parcel) && reached.insert(parcel.clone()) {
                pending.extend(object_utils::load_parcel(&parcel)?.parents);
            }
        }

        Ok(reached)
    };

    // Taking the served head itself adds nothing past it that any exemption leaves to judge.
    if served == Some(new) && applied.is_empty() {
        return Ok(None);
    }

    let mut reached = ancestry(expected, &HashSet::new())?;
    let chain = ancestry(served, &reached)?;
    let mut pending = vec![new.to_string()];

    while let Some(hash) = pending.pop() {
        if !reached.insert(hash.clone()) {
            continue;
        }

        // The served chain's ancestors are in it or reached already, so nothing below it is judged
        // by `set`.
        let judged = match chain.contains(&hash) {
            true if applied.is_empty() => continue,
            true => applied,
            false => set,
        };

        let parcel = object_utils::load_parcel(&hash)?;
        let mut inherited: HashSet<String> = HashSet::new();

        for parent in &parcel.parents {
            inherited.extend(tree_objects(&object_utils::load_parcel(parent)?.tree_hash)?);
        }

        if let Some(named) = tree_adds(&parcel.tree_hash, &inherited, judged)? {
            return Ok(Some(Added { named, served: chain.contains(&hash) }));
        }

        pending.extend(parcel.parents);
    }

    Ok(None)
}

/// Every object a meta parcel's tree holds: its subtrees and files, at any depth.
fn tree_objects(tree: &str) -> Result<HashSet<String>, String> {
    let mut objects: HashSet<String> = HashSet::new();
    let mut pending = vec![tree.to_string()];

    while let Some(hash) = pending.pop() {
        if !objects.insert(hash.clone()) {
            continue;
        }

        let loaded = object_utils::load_tree_shared(&hash)?;
        objects.extend(loaded.get_files().map(|(_, file)| file.hash.clone()));
        pending.extend(loaded.get_subtrees().map(|(_, subtree)| subtree.hash.clone()));
    }

    Ok(objects)
}

/// The first file under meta tree `tree` naming a redacted object that `inherited` (everything
/// its parents' trees hold) does not. A subtree its parents hold is skipped whole.
fn tree_adds(tree: &str, inherited: &HashSet<String>, set: &RedactedSet) -> Result<Option<Named>, String> {
    let mut pending = vec![(String::new(), tree.to_string())];

    while let Some((prefix, hash)) = pending.pop() {
        if inherited.contains(&hash) {
            continue;
        }

        let loaded = object_utils::load_tree_shared(&hash)?;

        for (name, file) in loaded.get_files() {
            if inherited.contains(&file.hash) {
                continue;
            }

            if let Some(redacted) = set.named_by(file, None)? {
                return Ok(Some((format!("{}{}", prefix, name), redacted)));
            }
        }

        pending.extend(loaded.get_subtrees()
            .map(|(name, subtree)| (format!("{}{}/", prefix, name), subtree.hash.clone())));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(hashes: &[char], chunk_lists: &[(char, &[char])]) -> RedactedSet {
        RedactedSet {
            hashes: hashes.iter().map(|c| c.to_string().repeat(64)).collect(),
            chunk_lists: chunk_lists.iter()
                .map(|(digest, owners)| (digest.to_string(), owners.iter().map(|c| c.to_string().repeat(64)).collect()))
                .collect(),
        }
    }

    // Revert: drop a chunk list another holds whole in `without` → the applied recipe's list is
    // lost with the exempt one's; or keep every owner in `filtered` → the exempt part claims the
    // applied recipe's list; or subtract only the other's chunk-list owners → a redaction it holds
    // by hash alone keeps its list here.
    #[test]
    fn a_chunk_list_two_redactions_share_is_held_while_either_is() {
        let applied = "a".repeat(64);
        let whole = set(&['a', 'b'], &[('x', &['a', 'b']), ('y', &['b'])]);

        let exempt = whole.filtered(|hash| hash != applied);
        assert_eq!(exempt.hashes, HashSet::from(["b".repeat(64)]));
        assert_eq!(exempt.chunk_lists, set(&[], &[('x', &['b']), ('y', &['b'])]).chunk_lists);

        let judged = whole.without(&exempt);
        assert_eq!(judged.hashes, HashSet::from([applied.clone()]));
        assert_eq!(judged.chunk_lists, set(&[], &[('x', &['a'])]).chunk_lists);

        // A redaction the other set holds by hash goes whole, though it read no chunk list for it;
        // a set holding only a chunk list still names something.
        let judged = set(&['a', 'b'], &[('x', &['a', 'b']), ('y', &['b'])]).without(&set(&['b'], &[]));
        assert_eq!(judged.chunk_lists, set(&[], &[('x', &['a'])]).chunk_lists);
        assert!(!set(&[], &[('x', &['a'])]).is_empty());

        let applied_part = set(&['a', 'b'], &[('x', &['a', 'b'])]).filtered(|hash| hash == applied);
        assert!(applied_part.names_hash(&applied) && !applied_part.names_hash(&"b".repeat(64)));
    }

    // Revert: skip the applied re-read in `holds` → a check that exempted a hash applied since
    // still holds.
    #[test]
    fn an_exemption_does_not_hold_once_its_hash_is_applied() {
        struct Cleanup(std::path::PathBuf);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let root = Cleanup(std::env::temp_dir().join(format!("forklift-redaction-holds-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&root.0);
        std::fs::create_dir_all(root.0.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = crate::globals::StorageRootScope::enter(&root.0);

        let exempted = "e".repeat(64);
        let check = RedactionCheck {
            namespace: pallet_utils::PalletNamespace::Meta,
            name: "tags".to_string(),
            expected: None,
            new: "f".repeat(64),
            redactions: None,
            skipped: HashSet::new(),
            adopted: false,
            exempted: HashSet::from([exempted.clone()]),
        };

        let lock = crate::util::lock_utils::RefLock::acquire().unwrap();
        assert!(check.holds(&lock).unwrap());

        std::fs::write(crate::globals::forklift_root().join("redactions-applied"),
                       format!("/forklift-redactions-applied 1\n{}\n", "d".repeat(64))).unwrap();
        assert!(check.holds(&lock).unwrap(), "another hash applied leaves it standing");

        std::fs::write(crate::globals::forklift_root().join("redactions-applied"),
                       format!("/forklift-redactions-applied 1\n{}\n", exempted)).unwrap();
        assert!(!check.holds(&lock).unwrap());
    }

    fn redaction() -> Redaction {
        Redaction {
            hash: "a".repeat(64),
            reason: "leaked key".to_string(),
            redacted_at: 7,
            chunks: None,
        }
    }

    #[test]
    fn a_redaction_round_trips_through_its_value() {
        let hash = "a".repeat(64);
        assert_eq!(parse_value(&hash, &redaction_value(&redaction())).unwrap(), redaction());

        let mut chunked = redaction();
        chunked.chunks = Some(chunk_list_digest(&["b".repeat(64), "c".repeat(64)]));
        assert_eq!(parse_value(&hash, &redaction_value(&chunked)).unwrap(), chunked);
    }

    #[test]
    fn a_value_is_parsed_strictly() {
        let hash = "a".repeat(64);

        let mut extra = redaction_value(&redaction());
        extra.insert("redactor", value("mallory"));
        assert!(parse_value(&hash, &extra).unwrap_err().contains("unexpected field \"redactor\""));

        let mut empty = redaction();
        empty.reason = "  ".to_string();
        assert!(parse_value(&hash, &redaction_value(&empty)).unwrap_err().contains("empty reason"));

        let mut long = redaction();
        long.reason = "r".repeat(MAX_REASON_BYTES + 1);
        assert!(parse_value(&hash, &redaction_value(&long)).unwrap_err().contains("at most"));

        let mut short = redaction();
        short.chunks = Some("abc".to_string());
        let error = parse_value(&hash, &redaction_value(&short)).unwrap_err();
        assert!(error.contains("not a full hash"), "{}", error);

        let mut untimed = redaction_value(&redaction());
        untimed.insert(FIELD_REDACTED_AT, value("yesterday"));
        assert!(parse_value(&hash, &untimed).unwrap_err().contains("not an integer"));
    }

    #[test]
    fn the_chunk_list_digest_keeps_order() {
        let (b, c) = ("b".repeat(64), "c".repeat(64));
        let digest = |chunks: &[&String]| {
            chunk_list_digest(&chunks.iter().map(|chunk| chunk.to_string()).collect::<Vec<_>>())
        };

        assert_eq!(digest(&[&b, &c]), digest(&[&b, &c]));
        assert_ne!(digest(&[&b, &c]), digest(&[&c, &b]));
        assert_ne!(digest(&[&b]), digest(&[&b, &b]));
        assert!(object_utils::is_object_hash(&digest(&[&b])));
    }

    #[test]
    fn the_record_command_quotes_the_path_behind_the_head() {
        let command = |path: &str, windows: bool| record_command_when("abc123", path, windows);

        for windows in [false, true] {
            assert_eq!(command("dir/secret.env", windows), "forklift redact record abc123:dir/secret.env -m <reason>");
            assert_eq!(command("-x.env", windows), "forklift redact record abc123:-x.env -m <reason>");
        }

        assert_eq!(command("a b.env", false), "forklift redact record abc123:'a b.env' -m <reason>");
        assert_eq!(
            command("a b.env", true),
            "forklift redact record abc123:<path> -m <reason>, with that path quoted for your shell"
        );
    }

    #[test]
    fn a_redacted_hash_is_a_full_object_hash() {
        assert!(validate_redacted_hash(&"a".repeat(64)).is_ok());

        for bad in ["", "abc", &"a".repeat(63), &"g".repeat(64), "v1.0"] {
            assert!(validate_redacted_hash(bad).is_err(), "{:?} must be refused", bad);
        }
    }
}
