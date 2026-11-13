//! Power loss (Linux): what a machine could hold after losing power at any moment of the faulted
//! command, rebuilt from the counting run's trace and graded like a kill.
//!
//! The fixture, the scratch area as it stands once the faulted record's files are written, is taken
//! as written and synced whole. The traced calls of every line of the faulted command are then
//! replayed in order, children's included, over a model of the area that carries two states across
//! lines: the volatile one, what the calls produced, and the durable one, keyed by inode: each
//! file's last synced contents and each directory's last synced entries. A file fsync promotes that
//! file's contents and mode, an fdatasync its contents only; a directory fsync promotes its whole entry set, entries an earlier line left
//! unsynced included; `sync`, and `syncfs` through a descriptor into the area, promote
//! everything. Files and folders are made under the umask in force. A rename is durable once its parent
//! is synced, and an entry survives only if its own directory does by the same rule, since the
//! durable tree is walked from the root: a new directory whose entry was never synced vanishes
//! with everything synced inside it.
//!
//! A crash lands at every kill point of every line, before the call, before every other sync,
//! children's included, and after each line exits; the state after the last line is the one the
//! tool acknowledged. A crash state that records the counting area's root stops the scenario as
//! path-bound, as the run's final tree would. These are POSIX-minimal
//! semantics. A real ext4 in data=ordered mode keeps more than this, so a loss graded here may
//! survive on a real machine; every row says so.
//!
//! The replay is checked against the run it models: once every line is replayed, the volatile
//! model must match the counting copy on disk, path by path, or the scenario's power loss is
//! refused. A call the model does not understand refuses too, never skipped.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::corpus::Rng;
use crate::faulted::{self, Graded, Programs};
use crate::kill::{self, Counting, Directories, Point, Settings};
use crate::report::{Category, Cell, Completeness, Row};
use crate::runner::RunError;
use crate::scenario::Scenario;
use crate::scrub::Scratch;
use crate::tool::Outcome;
use crate::trace::{self, Argument, Call, Class, Returned, Root};

/// What every power-loss row says beside its grade.
pub const CAVEAT: &str = "POSIX-minimal model: a real ext4 (data=ordered) keeps more, so a loss here may survive on a real machine";

/// A model inode.
type Ino = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    File { data: Vec<u8>, mode: u32 },
    Dir { entries: BTreeMap<String, Ino>, mode: u32 },
    Link { target: String },
}

impl Node {
    /// What a node holds durably the moment it is made: its kind and mode, no contents, no entries.
    fn born(&self) -> Node {
        match self {
            Node::File { mode, .. } => Node::File { data: Vec::new(), mode: *mode },
            Node::Dir { mode, .. } => Node::Dir { entries: BTreeMap::new(), mode: *mode },
            Node::Link { target } => Node::Link { target: target.clone() },
        }
    }
}

/// One path's entry in a tree.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Entry {
    File { data: Vec<u8>, mode: u32 },
    Dir { mode: u32 },
    Link { target: String },
}

/// A tree by root-relative, `/`-separated path; the root itself is `""`. Hard links read as
/// separate files.
pub type Image = BTreeMap<String, Entry>;

/// A digest of a tree, for telling states apart.
fn digest(image: &Image) -> String {
    let mut bytes = Vec::new();
    for (path, entry) in image {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        match entry {
            Entry::File { data, mode } => {
                bytes.push(b'f');
                bytes.extend_from_slice(&mode.to_le_bytes());
                bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
                bytes.extend_from_slice(data);
            }
            Entry::Dir { mode } => {
                bytes.push(b'd');
                bytes.extend_from_slice(&mode.to_le_bytes());
            }
            Entry::Link { target } => {
                bytes.push(b'l');
                bytes.extend_from_slice(target.as_bytes());
                bytes.push(0);
            }
        }
    }
    crate::oracle::digest(&bytes)
}

/// The two states of the modelled area.
#[derive(Clone, Debug)]
pub struct Model {
    volatile: BTreeMap<Ino, Node>,
    durable: BTreeMap<Ino, Node>,
    root: Ino,
    next: Ino,
    /// Bumped by every promotion, so an unchanged durable state is not walked again.
    version: u64,
    /// The index of the call being replayed in its line.
    now: usize,
    /// Per node, the last call in this line that changed it.
    changed: BTreeMap<Ino, usize>,
    /// Files cut shorter since their last sync: a crash may keep the cut and lose what followed.
    truncated: BTreeSet<Ino>,
}

impl Model {
    /// A model of `image`, all of it durable: the fixture, written and synced whole.
    pub fn of(image: &Image) -> Result<Model, String> {
        let Some(Entry::Dir { mode }) = image.get("") else { return Err("the fixture has no root folder".to_string()) };
        let mut model = Model { volatile: BTreeMap::new(), durable: BTreeMap::new(), root: 0, next: 1, version: 0, now: 0, changed: BTreeMap::new(), truncated: BTreeSet::new() };
        model.volatile.insert(0, Node::Dir { entries: BTreeMap::new(), mode: *mode });
        // A parent sorts before everything below it, so it is always made first.
        for (path, entry) in image.iter().filter(|(path, _)| !path.is_empty()) {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            let parent = model.lookup(parent).ok_or_else(|| format!("the fixture lists {} without its folder", path))?;
            let node = match entry {
                Entry::File { data, mode } => Node::File { data: data.clone(), mode: *mode },
                Entry::Dir { mode } => Node::Dir { entries: BTreeMap::new(), mode: *mode },
                Entry::Link { target } => Node::Link { target: target.clone() },
            };
            model.create(parent, name, node)?;
        }
        model.durable = model.volatile.clone();
        Ok(model)
    }

    /// The node at a root-relative path in the volatile state, no link followed.
    fn lookup(&self, path: &str) -> Option<Ino> {
        let mut at = self.root;
        for part in path.split('/').filter(|part| !part.is_empty()) {
            at = *self.entries(at)?.get(part)?;
        }
        Some(at)
    }

    fn entries(&self, ino: Ino) -> Option<&BTreeMap<String, Ino>> {
        match self.volatile.get(&ino)? {
            Node::Dir { entries, .. } => Some(entries),
            _ => None,
        }
    }

    /// Marks `ino` changed by the call being replayed.
    fn touch(&mut self, ino: Ino) {
        self.changed.insert(ino, self.now);
    }

    /// Refuses a sync of `ino` (every node when `None`) entered at `entered` when a call that
    /// finished while it ran changed what it syncs: whether the kernel's sync covered that change
    /// is not known, and promoting it would claim a durability nobody promised.
    fn overlapped(&self, ino: Option<Ino>, entered: usize) -> Result<(), String> {
        let late = |at: &usize| *at >= entered && *at < self.now;
        let overlapping = match ino {
            Some(ino) => self.changed.get(&ino).is_some_and(late),
            None => self.changed.values().any(late),
        };
        if overlapping {
            return Err("a sync overlapped a change to what it syncs, which the model does not decide".to_string());
        }
        Ok(())
    }

    fn entries_mut(&mut self, ino: Ino) -> Result<&mut BTreeMap<String, Ino>, String> {
        self.touch(ino);
        match self.volatile.get_mut(&ino) {
            Some(Node::Dir { entries, .. }) => Ok(entries),
            _ => Err("the replay reached a folder it holds as no folder".to_string()),
        }
    }

    fn create(&mut self, parent: Ino, name: &str, node: Node) -> Result<Ino, String> {
        let ino = self.next;
        self.next += 1;
        self.durable.insert(ino, node.born());
        self.volatile.insert(ino, node);
        self.entries_mut(parent)?.insert(name.to_string(), ino);
        Ok(ino)
    }

    /// An fsync of `ino`: a file's contents, or a folder's entries, become durable; with
    /// `data_only` (fdatasync) a file's mode keeps its durable value, since retrieving the data
    /// does not need it.
    fn sync(&mut self, ino: Ino, data_only: bool) {
        if let Some(node) = self.volatile.get(&ino) {
            if !matches!(node, Node::Link { .. }) {
                let mut node = node.clone();
                if let (true, Node::File { mode, .. }, Some(Node::File { mode: kept, .. })) = (data_only, &mut node, self.durable.get(&ino)) {
                    *mode = *kept;
                }
                self.durable.insert(ino, node);
                self.truncated.remove(&ino);
                self.version += 1;
            }
        }
    }

    fn sync_all(&mut self) {
        self.durable = self.volatile.clone();
        self.truncated.clear();
        self.version += 1;
    }

    /// What survives a crash now: the durable state, walked from the root.
    pub fn durable_image(&self) -> Image {
        image(&self.durable, self.root)
    }

    /// What the calls produced.
    pub fn volatile_image(&self) -> Image {
        image(&self.volatile, self.root)
    }

    /// A seeded state between the durable and the volatile one: each folder entry the two disagree
    /// on is taken from either, and each file whose contents they disagree on keeps its durable
    /// contents, takes its volatile ones, or is torn: a file cut shorter since its sync comes back
    /// as a prefix of its volatile contents, empty included, any other grows past its synced size
    /// with a zero-filled or torn tail or is torn in place.
    ///
    /// Entries are drawn one by one, so an unsynced rename can come back under neither name. That
    /// is deliberate: POSIX promises nothing of an unsynced rename across a crash, and the durable
    /// state already drops one whose folders were synced apart; a journalled file system keeps
    /// one name, which is what the caveat on every row says.
    pub fn variant(&self, rng: &mut Rng) -> Image {
        let mut out = Image::new();
        let mut seen = BTreeSet::new();
        let mut stack = vec![(String::new(), self.root)];
        while let Some((path, ino)) = stack.pop() {
            let (durable, volatile) = (self.durable.get(&ino), self.volatile.get(&ino));
            match (durable, volatile) {
                (Some(Node::Dir { entries: kept, mode }), Some(Node::Dir { entries: made, .. })) => {
                    if !seen.insert(ino) {
                        continue;
                    }
                    out.insert(path.clone(), Entry::Dir { mode: *mode });
                    let names: BTreeSet<&String> = kept.keys().chain(made.keys()).collect();
                    for name in names {
                        let (kept, made) = (kept.get(name), made.get(name));
                        let chosen = if kept == made || rng.below(2) == 0 { kept } else { made };
                        if let Some(child) = chosen {
                            stack.push((join(&path, name), *child));
                        }
                    }
                }
                (Some(Node::File { data: kept, mode }), Some(Node::File { data: made, .. })) => {
                    let data = if kept == made {
                        kept.clone()
                    } else {
                        match rng.below(3) {
                            0 => kept.clone(),
                            1 => made.clone(),
                            _ if self.truncated.contains(&ino) => made[..rng.below(made.len() as u64 + 1) as usize].to_vec(),
                            _ => grown(kept, made, rng),
                        }
                    };
                    out.insert(path, Entry::File { data, mode: *mode });
                }
                (Some(Node::Link { target }), _) => {
                    out.insert(path, Entry::Link { target: target.clone() });
                }
                _ => {}
            }
        }
        out
    }
}

/// `kept` grown toward `made`'s size: a length past the synced one, its tail zeros or `made`'s
/// bytes. A file the volatile state did not grow is torn in place instead: a prefix of `made` over
/// the synced bytes.
fn grown(kept: &[u8], made: &[u8], rng: &mut Rng) -> Vec<u8> {
    // Not grown: overwritten in place, and torn there, the first bytes new and the rest synced.
    if made.len() <= kept.len() {
        let mut out = kept.to_vec();
        let landed = rng.below(made.len() as u64 + 1) as usize;
        out[..landed].copy_from_slice(&made[..landed]);
        return out;
    }
    let length = kept.len() + 1 + rng.below((made.len() - kept.len()) as u64) as usize;
    let mut out = kept.to_vec();
    if rng.below(2) == 0 {
        out.resize(length, 0);
    } else {
        out.extend_from_slice(&made[kept.len()..length]);
    }
    out
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{}/{}", path, name) }
}

/// The tree `nodes` hold, walked from `root`, each folder once.
fn image(nodes: &BTreeMap<Ino, Node>, root: Ino) -> Image {
    let mut out = Image::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![(String::new(), root)];
    while let Some((path, ino)) = stack.pop() {
        match nodes.get(&ino) {
            Some(Node::File { data, mode }) => {
                out.insert(path, Entry::File { data: data.clone(), mode: *mode });
            }
            Some(Node::Link { target }) => {
                out.insert(path, Entry::Link { target: target.clone() });
            }
            Some(Node::Dir { entries, mode }) => {
                if !seen.insert(ino) {
                    continue;
                }
                out.insert(path.clone(), Entry::Dir { mode: *mode });
                for (name, child) in entries {
                    stack.push((join(&path, name), *child));
                }
            }
            None => {}
        }
    }
    out
}

/// Where a path lands in the volatile model.
#[derive(Debug, PartialEq, Eq)]
enum Located {
    /// Outside the scratch area: not modelled.
    Outside,
    /// The root itself.
    Root,
    /// A name in a folder, and what it names there, if anything.
    At { parent: Ino, name: String, ino: Option<Ino> },
    /// A folder on the way is missing, or is no folder.
    Missing,
}

/// An open file description.
#[derive(Clone, Debug)]
struct Description {
    /// The file, or None for what lies outside the area (a pipe, a terminal, another file system).
    ino: Option<Ino>,
    offset: u64,
    append: bool,
}

/// The replay of traced calls over a model.
pub struct Replay {
    pub model: Model,
    root: Root,
    descriptions: Vec<Description>,
    /// Descriptor tables by id, each descriptor's description with the index of the call that
    /// installed it, and the table each thread group uses.
    tables: BTreeMap<u32, BTreeMap<i64, (usize, usize)>>,
    table_of: BTreeMap<u32, u32>,
    /// The umask the traced command started under, and each thread group's as it stands.
    umask: u32,
    umasks: BTreeMap<u32, u32>,
}

/// Calls that make something durable.
const SYNCS: &[&str] = &["fsync", "fdatasync", "sync", "syncfs"];

/// Calls that change only what the model does not hold: times, owners, extended attributes, and a
/// range writeback that promises nothing durable.
const IGNORED: &[&str] = &[
    "utimensat", "utimes", "futimesat", "utime", "fchownat", "chown", "lchown", "fchown", "setxattr", "lsetxattr",
    "removexattr", "lremovexattr", "fsetxattr", "fremovexattr", "sync_file_range",
];

impl Replay {
    /// A replay over `model` of commands started under `umask`.
    pub fn new(model: Model, root: Root, umask: u32) -> Replay {
        Replay { model, root, descriptions: Vec::new(), tables: BTreeMap::new(), table_of: BTreeMap::new(), umask, umasks: BTreeMap::new() }
    }

    /// Replays one traced line, run in the folder `start`. `before` sees the model before each
    /// call, by the call's index.
    pub fn line(&mut self, calls: &[Call], start: &str, mut before: impl FnMut(usize, &Model)) -> Result<(), String> {
        // Every line is a new set of processes.
        self.descriptions.clear();
        self.tables.clear();
        self.table_of.clear();
        self.umasks.clear();
        self.model.changed.clear();
        let groups = trace::thread_groups(calls);
        let mut parents: BTreeMap<u32, (u32, bool)> = BTreeMap::new();
        for call in calls.iter().filter(|call| matches!(call.name.as_str(), "clone" | "clone3" | "fork" | "vfork")) {
            let Returned::Value(child) = &call.returned else { continue };
            let Ok(child) = child.parse::<u32>() else { continue };
            if child == 0 || call.arguments.iter().any(|argument| argument.contains("CLONE_THREAD")) {
                continue;
            }
            let parent = groups.get(&call.tid).copied().unwrap_or(call.tid);
            let shares = call.arguments.iter().any(|argument| argument.contains("CLONE_FILES"));
            parents.entry(groups.get(&child).copied().unwrap_or(child)).or_insert((parent, shares));
        }
        let mut directories = Directories::new(groups, start);
        for (index, call) in calls.iter().enumerate() {
            self.model.now = index;
            before(index, &self.model);
            let group = directories.group(call.tid);
            self.ensure(group, &parents, 0);
            // A close frees its number even when it fails (EINTR, EIO).
            if matches!(call.returned, Returned::Value(_)) || (call.name == "close" && matches!(call.returned, Returned::Failed(_))) {
                self.apply(call, group, &directories, &parents)
                    .map_err(|why| format!("{}({}): {}", call.name, call.arguments.join(", "), why))?;
            }
            directories.follow(call);
        }
        Ok(())
    }

    /// Gives `group` its descriptor table: its parent's own when the clone shared it, a copy of
    /// its parent's as it stands otherwise, an empty one for the traced command itself; and its
    /// parent's umask, or the one the command started under.
    fn ensure(&mut self, group: u32, parents: &BTreeMap<u32, (u32, bool)>, depth: usize) {
        if self.table_of.contains_key(&group) {
            return;
        }
        match parents.get(&group).copied() {
            Some((parent, shares)) if parent != group && depth < 64 => {
                self.ensure(parent, parents, depth + 1);
                let inherited = self.umasks[&parent];
                self.umasks.insert(group, inherited);
                let table = self.table_of[&parent];
                if shares {
                    self.table_of.insert(group, table);
                } else {
                    let copy = self.tables[&table].clone();
                    self.tables.insert(group, copy);
                    self.table_of.insert(group, group);
                }
            }
            _ => {
                self.tables.insert(group, BTreeMap::new());
                self.table_of.insert(group, group);
                self.umasks.insert(group, self.umask);
            }
        }
    }

    fn table(&mut self, group: u32) -> &mut BTreeMap<i64, (usize, usize)> {
        let id = self.table_of[&group];
        self.tables.get_mut(&id).expect("every table id names a table")
    }

    /// The description `fd` names in `group`'s table, judged by the `-y` path strace printed for
    /// it: None for what lies outside the area, an error for a descriptor into the area the replay
    /// never saw opened, since its writes would go nowhere.
    fn held(&mut self, group: u32, text: Option<&String>) -> Result<Option<usize>, String> {
        let Some(text) = text else { return Err("no descriptor".to_string()) };
        let (fd, target) = match trace::argument(text) {
            Argument::Descriptor { fd, target } => (fd, Some(target)),
            Argument::Other(number) => (number.parse().map_err(|_| format!("no descriptor in {}", text))?, None),
            _ => return Err(format!("no descriptor in {}", text)),
        };
        let into_area = target.as_deref().map(|target| self.root.holds(target));
        match self.table(group).get(&fd).map(|(description, _)| *description) {
            // strace names what the number is now: one it shows outside the area was reused for
            // something the table never saw opened (a pipe, a socket, a descriptor an exec
            // closed), so the table's entry is stale.
            Some(_) if into_area == Some(false) => {
                self.table(group).remove(&fd);
                Ok(None)
            }
            // One strace shows naming another file than the table's was reused through a route the
            // replay did not see; which file a write through it reaches is not known.
            Some(description) if self.descriptions[description].ino.is_some() => {
                let named = match target.as_deref() {
                    Some(target) if into_area == Some(true) => match self.locate(target, true)? {
                        Located::At { ino: Some(ino), .. } => Some(ino),
                        Located::Root => Some(self.model.root),
                        _ => None,
                    },
                    _ => None,
                };
                match named {
                    Some(ino) if Some(ino) != self.descriptions[description].ino => {
                        Err(format!("descriptor {} names {}, which is not the file the replay saw opened under it", fd, self.root.tokenized(target.as_deref().unwrap_or_default())))
                    }
                    _ => Ok(Some(description)),
                }
            }
            _ if into_area == Some(true) => Err(format!("descriptor {} into the area was never seen opened", fd)),
            _ => Ok(None),
        }
    }

    /// Where `absolute` lands in the volatile model, links on the way followed (the last one too
    /// when `follow`), up to the kernel's 40.
    fn locate(&self, absolute: &str, follow: bool) -> Result<Located, String> {
        let mut pending = absolute.to_string();
        let mut hops = 0;
        'path: loop {
            let tokenized = self.root.tokenized(&pending);
            let Some(rest) = tokenized.strip_prefix("<root>") else { return Ok(Located::Outside) };
            let parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
            if parts.is_empty() {
                return Ok(Located::Root);
            }
            let mut at = self.model.root;
            for (index, part) in parts.iter().enumerate() {
                let last = index + 1 == parts.len();
                let Some(entries) = self.model.entries(at) else { return Ok(Located::Missing) };
                let Some(&child) = entries.get(*part) else {
                    return Ok(if last { Located::At { parent: at, name: part.to_string(), ino: None } } else { Located::Missing });
                };
                match self.model.volatile.get(&child) {
                    Some(Node::Link { target }) if !last || follow => {
                        hops += 1;
                        if hops > 40 {
                            return Err(format!("too many links resolving {}", absolute));
                        }
                        let folder = format!("{}/{}", self.root.given, parts[..index].join("/"));
                        let mut next = trace::resolve(target, &folder);
                        for part in &parts[index + 1..] {
                            next = format!("{}/{}", next, part);
                        }
                        pending = next;
                        continue 'path;
                    }
                    _ if last => return Ok(Located::At { parent: at, name: part.to_string(), ino: Some(child) }),
                    Some(Node::Dir { .. }) => at = child,
                    _ => return Ok(Located::Missing),
                }
            }
            unreachable!("the last part returns");
        }
    }

    /// The node `absolute` names, which the call's success says exists.
    fn existing(&self, absolute: &str, follow: bool) -> Result<Option<Ino>, String> {
        match self.locate(absolute, follow)? {
            Located::Outside => Ok(None),
            Located::Root => Ok(Some(self.model.root)),
            Located::At { ino: Some(ino), .. } => Ok(Some(ino)),
            _ => Err(format!("the replay holds nothing at {}", self.root.tokenized(absolute))),
        }
    }

    /// The free name `absolute` names, which the call's success says it could make.
    fn free(&self, absolute: &str) -> Result<Option<(Ino, String)>, String> {
        match self.locate(absolute, false)? {
            Located::Outside => Ok(None),
            Located::At { parent, name, ino: None } => Ok(Some((parent, name))),
            Located::At { .. } | Located::Root => Err(format!("the replay already holds {}", self.root.tokenized(absolute))),
            Located::Missing => Err(format!("the replay holds no folder for {}", self.root.tokenized(absolute))),
        }
    }

    fn file(&mut self, ino: Ino) -> Result<&mut Vec<u8>, String> {
        self.model.touch(ino);
        match self.model.volatile.get_mut(&ino) {
            Some(Node::File { data, .. }) => Ok(data),
            _ => Err("a write to what the replay holds as no file".to_string()),
        }
    }

    fn apply(&mut self, call: &Call, group: u32, directories: &Directories, parents: &BTreeMap<u32, (u32, bool)>) -> Result<(), String> {
        let text = |index: usize| call.arguments.get(index);
        let raw = |index: usize| call.arguments.get(index).map(String::as_str).unwrap_or_default();
        // A path argument made absolute against its directory descriptor or the working directory.
        let path = |directory: Option<usize>, index: usize| -> Result<String, String> {
            let Some(Argument::Text(traced)) = text(index).map(|text| trace::argument(text)) else {
                return Err(format!("argument {} is no path", index));
            };
            let base = match directory.and_then(text).map(|text| trace::argument(text)) {
                Some(Argument::Descriptor { target, .. }) => target,
                Some(Argument::Cwd(Some(cwd))) => cwd,
                _ => directories.of(call.tid),
            };
            Ok(trace::resolve(&traced, &base))
        };
        let returned = match &call.returned {
            Returned::Value(value) => value.clone(),
            Returned::Failed(_) if call.name == "close" => String::new(),
            _ => return Ok(()),
        };
        let number = || -> Result<u64, String> {
            let digits: String = returned.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().map_err(|_| format!("no count in its return {}", returned))
        };
        match call.name.as_str() {
            "open" | "openat" | "creat" | "openat2" => {
                let (flags, mode) = match call.name.as_str() {
                    "open" => (raw(1).to_string(), mode_of(raw(2))),
                    "creat" => ("O_CREAT|O_WRONLY|O_TRUNC".to_string(), mode_of(raw(1))),
                    "openat" => (raw(2).to_string(), mode_of(raw(3))),
                    _ => (raw(2).to_string(), raw(2).split_once("mode=").map_or(0o644, |(_, rest)| mode_of(rest))),
                };
                let Argument::Descriptor { fd, target } = trace::argument(&returned) else {
                    return Err(format!("an open returned {}, a descriptor strace did not name", returned));
                };
                let ino = if self.root.holds(&target) {
                    if flags.contains("O_TMPFILE") {
                        return Err("an unnamed temp file is not modelled".to_string());
                    }
                    match self.locate(&target, false)? {
                        Located::Root => Some(self.model.root),
                        Located::At { ino: Some(ino), .. } => {
                            if flags.contains("O_TRUNC") {
                                self.model.touch(ino);
                                if let Node::File { data, .. } = self.model.volatile.get_mut(&ino).expect("located") {
                                    if !data.is_empty() {
                                        self.model.truncated.insert(ino);
                                    }
                                    data.clear();
                                }
                            }
                            Some(ino)
                        }
                        Located::At { parent, name, ino: None } if flags.contains("O_CREAT") => {
                            Some(self.model.create(parent, &name, Node::File { data: Vec::new(), mode: mode & !self.umasks[&group] })?)
                        }
                        _ => return Err(format!("the replay holds nothing at {}", self.root.tokenized(&target))),
                    }
                } else {
                    None
                };
                self.descriptions.push(Description { ino, offset: 0, append: flags.contains("O_APPEND") });
                let description = self.descriptions.len() - 1;
                let now = self.model.now;
                self.table(group).insert(fd, (description, now));
            }
            "dup" | "dup2" | "dup3" | "fcntl" => {
                if call.name == "fcntl" && raw(1) == "F_SETFL" {
                    // The status flags belong to the open file description, shared by every dup.
                    if let Some((description, _)) = fd_of(raw(0)).and_then(|fd| self.table(group).get(&fd).copied()) {
                        self.descriptions[description].append = raw(2).contains("O_APPEND");
                    }
                    return Ok(());
                }
                if call.name == "fcntl" && !raw(1).starts_with("F_DUPFD") {
                    return Ok(());
                }
                let old = fd_of(raw(0));
                let new = number()? as i64;
                let found = old.and_then(|old| self.table(group).get(&old).copied());
                let now = self.model.now;
                match found {
                    Some((description, _)) => self.table(group).insert(new, (description, now)),
                    None => self.table(group).remove(&new),
                };
            }
            // A close frees its number before it returns, so a descriptor another thread was
            // handed under that number while the close ran is not the one it closed.
            "close" => {
                if let Some(fd) = fd_of(raw(0)) {
                    if self.table(group).get(&fd).is_some_and(|(_, installed)| *installed < call.entered) {
                        self.table(group).remove(&fd);
                    }
                }
            }
            "close_range" => {
                // CLOSE_RANGE_UNSHARE gives the caller its own copy of the table first, so what it
                // closes the processes it shared with keep.
                if raw(2).contains("CLOSE_RANGE_UNSHARE") && self.table_of[&group] != group {
                    let copy = self.table(group).clone();
                    self.tables.insert(group, copy);
                    self.table_of.insert(group, group);
                }
                if !raw(2).contains("CLOSE_RANGE_CLOEXEC") {
                    let first = fd_of(raw(0)).unwrap_or(0);
                    let last = if raw(1).starts_with('~') { i64::MAX } else { fd_of(raw(1)).unwrap_or(i64::MAX) };
                    let entered = call.entered;
                    self.table(group).retain(|fd, (_, installed)| *fd < first || *fd > last || *installed >= entered);
                }
            }
            "read" | "readv" | "lseek" => {
                if let Some(description) = self.held(group, text(0))? {
                    let count = number()?;
                    let held = &mut self.descriptions[description];
                    held.offset = if call.name == "lseek" { count } else { held.offset + count };
                }
            }
            "write" | "writev" | "pwrite64" | "pwritev" | "pwritev2" => {
                let Some(description) = self.held(group, text(0))? else { return Ok(()) };
                let count = number()? as usize;
                if let Some(why) = &call.unreadable {
                    return Err(why.clone());
                }
                if call.data.len() < count {
                    return Err(format!("the trace dumped {} of its {} bytes: trace with -e write=all", call.data.len(), count));
                }
                // pwritev2 at offset -1 writes at the description's offset, like writev; with
                // RWF_APPEND it appends whatever the description's own flag says.
                let positioned = call.name.starts_with('p') && !(call.name == "pwritev2" && raw(3) == "-1");
                let appending = call.name == "pwritev2" && raw(4).contains("RWF_APPEND");
                let held = self.descriptions[description].clone();
                let ino = held.ino.expect("held returns only descriptions into the area");
                let data = self.file(ino)?;
                // Linux appends a positioned write on an O_APPEND description too (pwrite(2), BUGS).
                let at = if appending || held.append {
                    data.len()
                } else if positioned {
                    fd_of(raw(3)).ok_or_else(|| "no offset".to_string())? as usize
                } else {
                    held.offset as usize
                };
                if data.len() < at + count {
                    data.resize(at + count, 0);
                }
                data[at..at + count].copy_from_slice(&call.data[..count]);
                if !positioned {
                    self.descriptions[description].offset = (at + count) as u64;
                }
            }
            "ftruncate" | "truncate" => {
                let ino = if call.name == "ftruncate" {
                    self.held(group, text(0))?.and_then(|description| self.descriptions[description].ino)
                } else {
                    self.existing(&path(None, 0)?, true)?
                };
                if let Some(ino) = ino {
                    let length = fd_of(raw(1)).ok_or_else(|| "no length".to_string())? as usize;
                    let data = self.file(ino)?;
                    let cut = length < data.len();
                    data.resize(length, 0);
                    if cut {
                        self.model.truncated.insert(ino);
                    }
                }
            }
            "fsync" | "fdatasync" => {
                if let Some(description) = self.held(group, text(0))? {
                    let ino = self.descriptions[description].ino.expect("held returns only descriptions into the area");
                    self.model.overlapped(Some(ino), call.entered)?;
                    self.model.sync(ino, call.name == "fdatasync");
                }
            }
            // syncfs promises only the file system its descriptor lies on; one outside the area
            // promises the area nothing.
            "sync" | "syncfs" => {
                if call.name == "sync" || self.held(group, text(0))?.is_some() {
                    self.model.overlapped(None, call.entered)?;
                    self.model.sync_all();
                }
            }
            "mkdir" | "mkdirat" => {
                let (target, mode) = if call.name == "mkdir" { (path(None, 0)?, mode_of(raw(1))) } else { (path(Some(0), 1)?, mode_of(raw(2))) };
                if let Some((parent, name)) = self.free(&target)? {
                    self.model.create(parent, &name, Node::Dir { entries: BTreeMap::new(), mode: mode & !self.umasks[&group] })?;
                }
            }
            "rename" | "renameat" | "renameat2" => {
                let (from, to, flags) = match call.name.as_str() {
                    "rename" => (path(None, 0)?, path(None, 1)?, ""),
                    "renameat" => (path(Some(0), 1)?, path(Some(2), 3)?, ""),
                    _ => (path(Some(0), 1)?, path(Some(2), 3)?, raw(4)),
                };
                self.rename(&from, &to, flags)?;
            }
            "unlink" | "unlinkat" | "rmdir" => {
                let target = if call.name == "unlinkat" { path(Some(0), 1)? } else { path(None, 0)? };
                match self.locate(&target, false)? {
                    Located::Outside => {}
                    Located::At { parent, name, ino: Some(_) } => {
                        self.model.entries_mut(parent)?.remove(&name);
                    }
                    _ => return Err(format!("the replay holds nothing at {}", self.root.tokenized(&target))),
                }
            }
            "link" | "linkat" => {
                let (from, to, follow) = if call.name == "link" {
                    (path(None, 0)?, path(None, 1)?, false)
                } else {
                    (path(Some(0), 1)?, path(Some(2), 3)?, raw(4).contains("AT_SYMLINK_FOLLOW"))
                };
                let (source, place) = (self.existing(&from, follow)?, self.free(&to)?);
                match (source, place) {
                    (None, None) => {}
                    (Some(ino), Some((parent, name))) => {
                        self.model.entries_mut(parent)?.insert(name, ino);
                    }
                    _ => return Err("a hard link across the edge of the area".to_string()),
                }
            }
            "symlink" | "symlinkat" => {
                let Some(Argument::Text(target)) = text(0).map(|text| trace::argument(text)) else { return Err("no link target".to_string()) };
                let at = if call.name == "symlink" { path(None, 1)? } else { path(Some(1), 2)? };
                if let Some((parent, name)) = self.free(&at)? {
                    self.model.create(parent, &name, Node::Link { target })?;
                }
            }
            "chmod" | "fchmodat" | "fchmodat2" | "fchmod" => {
                let (ino, mode) = match call.name.as_str() {
                    "fchmod" => (self.held(group, text(0))?.and_then(|description| self.descriptions[description].ino), mode_of(raw(1))),
                    "chmod" => (self.existing(&path(None, 0)?, true)?, mode_of(raw(1))),
                    _ => (self.existing(&path(Some(0), 1)?, !raw(3).contains("AT_SYMLINK_NOFOLLOW"))?, mode_of(raw(2))),
                };
                if let Some(ino) = ino {
                    self.model.touch(ino);
                }
                if let Some(Node::File { mode: held, .. } | Node::Dir { mode: held, .. }) = ino.and_then(|ino| self.model.volatile.get_mut(&ino)) {
                    *held = mode;
                }
            }
            "clone" | "clone3" | "fork" | "vfork" => {
                // The child's table is its parent's as of now, unless the child already ran.
                if let Ok(child) = returned.parse::<u32>() {
                    if child != 0 && !call.arguments.iter().any(|argument| argument.contains("CLONE_THREAD")) {
                        let child = directories.group(child);
                        self.ensure(child, parents, 0);
                    }
                }
            }
            "umask" => {
                let mask = u32::from_str_radix(raw(0).trim_start_matches("0o"), 8).map_err(|_| format!("no mask in {}", raw(0)))?;
                self.umasks.insert(group, mask & 0o777);
            }
            name if IGNORED.contains(&name) => {}
            _ => match trace::touch(call) {
                Ok(touched) if touched.class == Class::Inert => {}
                Ok(touched) => {
                    let inside = touched.operands.iter().any(|operand| self.root.holds(&directories.resolved(operand, call.tid)));
                    if inside || touched.operands.is_empty() {
                        return Err("a change the power-loss model does not replay".to_string());
                    }
                }
                Err(unsound) => return Err(unsound.to_string()),
            },
        }
        Ok(())
    }

    fn rename(&mut self, from: &str, to: &str, flags: &str) -> Result<(), String> {
        let (source, target) = (self.locate(from, false)?, self.locate(to, false)?);
        match (&source, &target) {
            (Located::Outside, Located::Outside) => return Ok(()),
            (Located::Outside, _) | (_, Located::Outside) => return Err("a rename across the edge of the area".to_string()),
            _ => {}
        }
        if flags.contains("RENAME_WHITEOUT") {
            return Err("a whiteout rename is not modelled".to_string());
        }
        let Located::At { parent: from_parent, name: from_name, ino: Some(moved) } = source else {
            return Err(format!("the replay holds nothing at {}", self.root.tokenized(from)));
        };
        let Located::At { parent: to_parent, name: to_name, ino: replaced } = target else {
            return Err(format!("the replay holds no folder for {}", self.root.tokenized(to)));
        };
        if flags.contains("RENAME_EXCHANGE") {
            let replaced = replaced.ok_or_else(|| "an exchange with nothing".to_string())?;
            self.model.entries_mut(from_parent)?.insert(from_name, replaced);
            self.model.entries_mut(to_parent)?.insert(to_name, moved);
            return Ok(());
        }
        // A rename onto another link to the same file does nothing.
        if replaced == Some(moved) {
            return Ok(());
        }
        self.model.entries_mut(from_parent)?.remove(&from_name);
        self.model.entries_mut(to_parent)?.insert(to_name, moved);
        Ok(())
    }
}

/// A mode as strace prints it, octal; 0644 when it printed none.
fn mode_of(text: &str) -> u32 {
    let digits: String = text.trim().chars().take_while(|c| c.is_digit(8)).collect();
    u32::from_str_radix(&digits, 8).unwrap_or(0o644) & 0o7777
}

/// A descriptor or a number, as the argument's leading digits.
fn fd_of(text: &str) -> Option<i64> {
    let digits: String = text.trim().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Reads the tree under `root` as it stands, links as links. A name or link target that is not
/// UTF-8 is an error, never read lossily into another name.
pub fn scan(root: &Path) -> std::io::Result<Image> {
    let mut out = Image::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((at, path)) = stack.pop() {
        let metadata = std::fs::symlink_metadata(&at)?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            let target = std::fs::read_link(&at)?;
            let target = target.to_str().ok_or_else(|| std::io::Error::other(format!("{} links to a path that is not UTF-8, which the model does not hold", at.display())))?;
            out.insert(path, Entry::Link { target: target.to_string() });
        } else if kind.is_dir() {
            for entry in std::fs::read_dir(&at)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| std::io::Error::other(format!("{} is not UTF-8, which the model does not hold", entry.path().display())))?;
                stack.push((entry.path(), join(&path, name)));
            }
            out.insert(path, Entry::Dir { mode: mode(&metadata, 0o755) });
        } else if kind.is_file() {
            out.insert(path, Entry::File { data: std::fs::read(&at)?, mode: mode(&metadata, 0o644) });
        } else {
            return Err(std::io::Error::other(format!("{} is neither file, folder nor link", at.display())));
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn mode(metadata: &std::fs::Metadata, _: u32) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode(_: &std::fs::Metadata, default: u32) -> u32 {
    default
}

/// The first path where the replay's tree and the run's differ, said plainly: by kind, by a file's
/// bytes, by permission bits, or by a link's target.
pub fn difference(replayed: &Image, ran: &Image) -> Option<String> {
    let permissions = |mode: &u32| mode & 0o777;
    let paths: BTreeSet<&String> = replayed.keys().chain(ran.keys()).collect();
    for path in paths {
        let shown = if path.is_empty() { "the root" } else { path.as_str() };
        let differs = match (replayed.get(path), ran.get(path)) {
            (Some(_), None) => Some("the replay holds it and the run left none".to_string()),
            (None, Some(_)) => Some("the run left it and the replay holds none".to_string()),
            (Some(Entry::Dir { mode: m }), Some(Entry::Dir { mode: n })) => (permissions(m) != permissions(n))
                .then(|| format!("the replay makes it mode {:o} and the run left {:o}", permissions(m), permissions(n))),
            (Some(Entry::File { data: a, mode: m }), Some(Entry::File { data: b, mode: n })) => {
                if a != b {
                    Some(format!("the replay holds {} bytes and the run left {} different ones", a.len(), b.len()))
                } else if permissions(m) != permissions(n) {
                    Some(format!("the replay makes it mode {:o} and the run left {:o}", permissions(m), permissions(n)))
                } else {
                    None
                }
            }
            (Some(Entry::Link { target: a }), Some(Entry::Link { target: b })) => (a != b).then(|| format!("the replay links it to {} and the run to {}", a, b)),
            _ => Some("the replay and the run hold different kinds there".to_string()),
        };
        if let Some(why) = differs {
            return Some(format!("{}: {}", shown, why));
        }
    }
    None
}

/// Writes `image` into `root`, emptied first. Folder modes are set last, deepest first, so a
/// read-only folder does not stop its own contents being written.
pub fn materialize(image: &Image, root: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_tree(&entry.path())?;
        } else {
            #[allow(clippy::disallowed_methods)] // a scratch copy the harness made itself; no retry is wanted
            std::fs::remove_file(entry.path())?;
        }
    }
    for (path, entry) in image.iter().filter(|(path, _)| !path.is_empty()) {
        let at = root.join(path);
        match entry {
            Entry::Dir { .. } => std::fs::create_dir(&at)?,
            Entry::File { data, mode } => {
                std::fs::write(&at, data)?;
                set_mode(&at, *mode)?;
            }
            #[cfg(unix)]
            Entry::Link { target } => std::os::unix::fs::symlink(target, &at)?,
            #[cfg(not(unix))]
            Entry::Link { .. } => return Err(std::io::Error::other(format!("a link at {} is not written on this platform", path))),
        }
    }
    for (path, entry) in image.iter().rev() {
        if let Entry::Dir { mode } = entry {
            set_mode(&root.join(path), *mode)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Removes a tree the harness wrote itself, its folders made writable first, since a crash state
/// can hold a read-only folder whose entries could not be removed otherwise. Links are removed,
/// never followed.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    fn open_up(path: &Path) -> std::io::Result<()> {
        if !std::fs::symlink_metadata(path)?.is_dir() {
            return Ok(());
        }
        set_mode(path, 0o700)?;
        for entry in std::fs::read_dir(path)? {
            open_up(&entry?.path())?;
        }
        Ok(())
    }
    open_up(path)?;
    #[allow(clippy::disallowed_methods, reason = "a scratch copy the harness made itself, under its own output directory")]
    std::fs::remove_dir_all(path)
}

/// When a crash lands.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "at", rename_all = "kebab-case")]
pub enum Moment {
    /// At a kill point, before its call.
    During { point: Point },
    /// Before a sync no kill point stands at, a child's or another thread's: the durable state
    /// changes only at a sync, so these with the kill points reach every one the line passes.
    Synced { line: usize, command: String, call: usize, name: String },
    /// After a line of the faulted command exited.
    After { line: usize, command: String },
}

impl Moment {
    /// The step trail a crash here is graded with: how many lines finished before it, and whether
    /// the next one was cut short. Two moments with one trail grade one state alike.
    fn trail(&self) -> (usize, bool) {
        match self {
            Moment::During { point } => (point.line, true),
            Moment::Synced { line, .. } => (*line, true),
            Moment::After { line, .. } => (line + 1, false),
        }
    }

    fn label(&self) -> String {
        match self {
            Moment::During { point } => format!("line {} {} {}#{} {}", point.line + 1, point.command, point.name, point.when, point.base),
            Moment::Synced { line, command, call, name } => format!("line {} {} before {} at traced call {}", line + 1, command, name, call + 1),
            Moment::After { line, command } => format!("after line {} {}", line + 1, command),
        }
    }
}

/// One state a crash can leave.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Crash {
    /// The first moment that leaves it.
    pub moment: Moment,
    /// Whether the faulted command had finished every line: the record was acknowledged.
    pub acknowledged: bool,
    /// How many crash moments leave this same state.
    pub shared: usize,
    /// For a seeded state between the durable and the volatile one, its number.
    pub variant: Option<usize>,
    #[serde(skip)]
    pub image: std::sync::Arc<Image>,
}

/// One traced line as the crash enumeration reads it.
pub struct Line<'a> {
    pub calls: &'a [Call],
    pub command: String,
    /// The kill points in it, by their call index.
    pub points: Vec<&'a Point>,
}

/// Every distinct state a crash can leave in `lines`, replayed over `fixture` from the folder
/// `start` under `root`, plus `variants` seeded states drawn at seeded moments. The last element of
/// the result's pair is the volatile tree the replay ended with, for the check against the run.
pub fn crashes(fixture: &Image, root: &Root, start: &str, umask: u32, lines: &[Line], variants: usize, seed: u64) -> Result<(Vec<Crash>, Image), String> {
    let mut replay = Replay::new(Model::of(fixture)?, root.clone(), umask);
    let synced = |line: &Line| -> BTreeSet<usize> {
        let at_points: BTreeSet<usize> = line.points.iter().map(|point| point.call).collect();
        line.calls.iter().enumerate()
            .filter(|(index, call)| SYNCS.contains(&call.name.as_str()) && matches!(call.returned, Returned::Value(_)) && !at_points.contains(index))
            .map(|(index, _)| index).collect()
    };
    let moments: usize = lines.iter().map(|line| line.points.len() + synced(line).len() + 1).sum();
    let mut rng = Rng::new(seed);
    // The moments a variant is drawn at, with repeats: each draw is its own variant.
    let mut draws: BTreeMap<usize, usize> = BTreeMap::new();
    for _ in 0..variants {
        if moments > 0 {
            *draws.entry(rng.below(moments as u64) as usize).or_default() += 1;
        }
    }
    let mut collector = Collector { crashes: Vec::new(), seen: BTreeMap::new(), cached: None, draws, rng, moment: 0, drawn: 0 };
    let last = lines.len().checked_sub(1).ok_or_else(|| "the faulted command has no lines".to_string())?;
    for (index, line) in lines.iter().enumerate() {
        let here: BTreeMap<usize, &Point> = line.points.iter().map(|point| (point.call, *point)).collect();
        let syncs = synced(line);
        replay.line(line.calls, start, |call, model| {
            if let Some(point) = here.get(&call) {
                collector.record(Moment::During { point: (*point).clone() }, model, false);
            } else if syncs.contains(&call) {
                let moment = Moment::Synced { line: index, command: line.command.clone(), call, name: line.calls[call].name.clone() };
                collector.record(moment, model, false);
            }
        }).map_err(|why| format!("line {} {}: {}", index + 1, line.command, why))?;
        collector.record(Moment::After { line: index, command: line.command.clone() }, &replay.model, index == last);
    }
    Ok((collector.crashes, replay.model.volatile_image()))
}

struct Collector {
    crashes: Vec<Crash>,
    /// Each state's crash, by its tree's digest, whether it was acknowledged, and its step trail.
    seen: BTreeMap<(String, bool, (usize, bool)), usize>,
    /// The durable tree of the last version walked.
    cached: Option<(u64, std::sync::Arc<Image>, String)>,
    draws: BTreeMap<usize, usize>,
    rng: Rng,
    /// The moment being recorded, counted from 0.
    moment: usize,
    drawn: usize,
}

impl Collector {
    fn record(&mut self, moment: Moment, model: &Model, acknowledged: bool) {
        if self.cached.as_ref().is_none_or(|(version, _, _)| *version != model.version) {
            let image = model.durable_image();
            let key = digest(&image);
            self.cached = Some((model.version, std::sync::Arc::new(image), key));
        }
        let (_, image, key) = self.cached.as_ref().expect("cached above");
        let (image, key) = (image.clone(), key.clone());
        self.add(moment.clone(), image, key, acknowledged, None);
        for _ in 0..self.draws.get(&self.moment).copied().unwrap_or_default() {
            let image = model.variant(&mut self.rng);
            let key = digest(&image);
            if !self.seen.contains_key(&(key.clone(), acknowledged, moment.trail())) {
                self.drawn += 1;
                self.add(moment.clone(), std::sync::Arc::new(image), key, acknowledged, Some(self.drawn));
            }
        }
        self.moment += 1;
    }

    fn add(&mut self, moment: Moment, image: std::sync::Arc<Image>, digest: String, acknowledged: bool, variant: Option<usize>) {
        match self.seen.get(&(digest.clone(), acknowledged, moment.trail())) {
            Some(&at) if variant.is_none() => {
                let crash = &mut self.crashes[at];
                // A durable state an earlier draw already found is no variant: it happens with no
                // torn write at all, so it is reported under this real moment.
                if crash.variant.is_some() {
                    (crash.moment, crash.variant, crash.shared) = (moment, None, 0);
                }
                crash.shared += 1;
            }
            Some(_) => {}
            None => {
                self.seen.insert((digest, acknowledged, moment.trail()), self.crashes.len());
                self.crashes.push(Crash { moment, acknowledged, shared: 1, variant, image });
            }
        }
    }
}

/// One crash state graded.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GradedCrash {
    pub crash: Crash,
    pub graded: Graded,
}

/// Every crash state of one scenario's faulted command.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ScenarioPower {
    pub scenario: String,
    /// Why the scenario's power loss was not run, when it was not.
    pub not_run: Option<String>,
    /// What refused the replay: unmodelled calls, a replay that does not reproduce the run.
    pub refusals: Vec<String>,
    /// States whose grading failed to happen at all, with why.
    pub errors: Vec<String>,
    pub crashes: Vec<GradedCrash>,
}

/// Grades every state a power loss during `scenario`'s faulted command can leave, `variants`
/// seeded ones among them, from the run [`kill::count`] counted in `template` with its writes
/// dumped. Each state is written into its own area beside the template, `<template>-power-<n>`.
pub fn run(scenario: &Scenario, programs: &Programs, settings: &Settings, template: &Scratch, counting: &Counting, variants: usize, seed: u64) -> Result<ScenarioPower, RunError> {
    let mut power = ScenarioPower { scenario: scenario.id.clone(), ..Default::default() };
    let counted = match counting {
        Counting::Counted(counted) => counted,
        Counting::NotRun(reason) => {
            power.not_run = Some(reason.clone());
            return Ok(power);
        }
    };
    let (points, _, refusals) = kill::points_of(counted, settings.schema);
    if let Some((refusals, not_run)) = kill::stopped(refusals, counted.path_bound.as_ref()) {
        (power.refusals, power.not_run) = (refusals, not_run);
        return Ok(power);
    }
    let mut lines = Vec::new();
    for (index, line) in counted.lines.iter().enumerate() {
        let command = kill::command_id(settings.schema, &line.arguments).map_err(RunError::Io)?;
        lines.push(Line { calls: &line.calls, command, points: points.iter().filter(|point| point.line == index).collect() });
    }
    let fixture = match &counted.fixture {
        Some(Ok(fixture)) => fixture,
        Some(Err(why)) => {
            power.refusals.push(format!("the counting area cannot be read whole: {}", why));
            return Ok(power);
        }
        None => return Err(RunError::Io("the faulted command was counted without its writes dumped".to_string())),
    };
    let start = counted.counting.work().to_string_lossy().into_owned();
    let umask = harness_umask().map_err(RunError::Io)?;
    let (crashes, replayed) = match crashes(fixture, &counted.counting_root, &start, umask, &lines, variants, seed) {
        Ok(found) => found,
        Err(refusal) => {
            power.refusals.push(refusal);
            return Ok(power);
        }
    };
    let ran = scan(counted.counting.root()).map_err(|error| RunError::Io(error.to_string()))?;
    let needles = [counted.counting_root.given.as_str(), counted.counting_root.canonical.as_str()];
    if let Some((refusals, not_run)) = stopped(&replayed, &ran, &crashes, &needles) {
        (power.refusals, power.not_run) = (refusals, not_run);
        return Ok(power);
    }

    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<GradedCrash, String>)>> = Mutex::new(Vec::new());
    let workers = settings.jobs.clamp(1, crashes.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some(crash) = crashes.get(at) else { break };
                let result = grade(scenario, counted, programs, settings, template, crash, at)
                    .map(|graded| GradedCrash { crash: crash.clone(), graded })
                    .map_err(|error| format!("{}: {}", crash.moment.label(), error));
                results.lock().expect("no worker panics holding the lock").push((at, result));
            });
        }
    });
    let mut results = results.into_inner().expect("no worker panics holding the lock");
    results.sort_by_key(|(at, _)| *at);
    for (_, result) in results {
        match result {
            Ok(graded) => power.crashes.push(graded),
            Err(error) => power.errors.push(error),
        }
    }
    Ok(power)
}

/// Whether the crash states stop before grading, as [`kill::stopped`] says: refused when the
/// replay does not reproduce the run, else not run when a state records the counting area's root.
/// The run's final tree was checked for that root when counted; a state between holds files the
/// run later removed, and one naming the counting area would tie its copy to that area.
fn stopped(replayed: &Image, ran: &Image, crashes: &[Crash], needles: &[&str]) -> Option<(Vec<String>, Option<String>)> {
    let refusals: Vec<String> = difference(replayed, ran).map(|difference| format!("the replay does not reproduce the run: {}", difference)).into_iter().collect();
    let bound = crashes.iter().find_map(|crash| bound(&crash.image, needles).map(|path| format!("path-bound: the crash state {} records the scratch area's root in {}", crash.moment.label(), path)));
    kill::stopped(refusals, bound.as_ref())
}

/// The first path in `image` whose contents or link target hold one of `needles`, as
/// [`kill::recorded_root`] judges a tree on disk.
fn bound(image: &Image, needles: &[&str]) -> Option<String> {
    image.iter().find_map(|(path, entry)| {
        let bytes = match entry {
            Entry::File { data, .. } => data.as_slice(),
            Entry::Link { target } => target.as_bytes(),
            Entry::Dir { .. } => return None,
        };
        needles.iter().any(|needle| kill::holds_root(bytes, needle.as_bytes())).then(|| path.clone())
    })
}

/// The umask this process runs under, which every traced command inherits; read from /proc, since
/// the call that reads it sets it too.
fn harness_umask() -> Result<u32, String> {
    let status = std::fs::read_to_string("/proc/self/status").map_err(|error| format!("reading /proc/self/status for the umask: {}", error))?;
    status.lines().find_map(|line| line.strip_prefix("Umask:")).and_then(|mask| u32::from_str_radix(mask.trim(), 8).ok())
        .ok_or_else(|| "/proc/self/status names no umask".to_string())
}

/// One crash state written into a fresh copy and graded: the lines before the crash stand as the
/// counting run ran them, a line the crash cut short as killed.
fn grade(scenario: &Scenario, counted: &kill::Counted, programs: &Programs, settings: &Settings, template: &Scratch, crash: &Crash, at: usize) -> Result<Graded, RunError> {
    let io = |error: std::io::Error| RunError::Io(error.to_string());
    // The state is the whole area, so the copy starts empty rather than as the template.
    let copy = Scratch::create(Path::new(&format!("{}-power-{}", template.root().display(), at))).map_err(|error| RunError::Io(error.to_string()))?;
    materialize(&crash.image, copy.root()).map_err(io)?;
    let environment = kill::environment_of(settings, &copy)?;
    let context = kill::context_of(scenario, &counted.corpus, programs, settings, &copy, &environment);
    let mut steps: Vec<Outcome> = Vec::new();
    match &crash.moment {
        Moment::During { point: Point { line, .. } } | Moment::Synced { line, .. } => {
            steps.extend(counted.lines[..*line].iter().map(|line| line.outcome.clone()));
            let mut cut = counted.lines[*line].outcome.clone();
            (cut.exit, cut.failure) = (None, None);
            cut.stdout.clear();
            cut.stderr.clear();
            steps.push(cut);
        }
        Moment::After { line, .. } => steps.extend(counted.lines[..=*line].iter().map(|line| line.outcome.clone())),
    }
    let graded = faulted::grade_after(&context, &counted.expected, counted.faulted, |_, first| {
        first.append(&mut steps);
        Ok(crash.acknowledged)
    })?;
    // A copy that graded well holds nothing to look at; one that did not is kept for diagnosis.
    if !graded.grade.is_failure() {
        let _ = remove_tree(copy.root());
    }
    Ok(graded)
}

/// What the power loss of a run adds to its report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub cell: Cell,
    pub rows: Vec<Row>,
    pub completeness: Vec<Completeness>,
}

/// The report's share of every scenario's power loss. A scenario reported not run is listed, and
/// fails nothing.
pub fn summarize(all: &[ScenarioPower]) -> Summary {
    let (mut rows, mut refusals, mut errors, mut not_run, mut missing) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for scenario in all {
        if let Some(reason) = &scenario.not_run {
            not_run.push(Completeness { check: format!("power loss of {}", scenario.scenario), cell: Cell::not_run(reason), missing: vec![] });
            continue;
        }
        refusals.extend(scenario.refusals.iter().map(|refusal| format!("{}: {}", scenario.scenario, refusal)));
        errors.extend(scenario.errors.iter().map(|error| format!("{}: {}", scenario.scenario, error)));
        if scenario.refusals.is_empty() && scenario.errors.is_empty() && scenario.crashes.is_empty() {
            missing.push(format!("{}: no crash state was graded", scenario.scenario));
        }
        for GradedCrash { crash, graded } in &scenario.crashes {
            let mut notes = vec![CAVEAT.to_string()];
            if crash.shared > 1 {
                notes.push(format!("{} crash moments leave this state", crash.shared));
            }
            if !graded.unextractable.is_empty() {
                notes.push(format!("unextractable remedies: {}", graded.unextractable.join("; ")));
            }
            let variant = crash.variant.map(|number| format!(" variant {}", number)).unwrap_or_default();
            rows.push(Row {
                scenario: format!("{} power {}{}", scenario.scenario, crash.moment.label(), variant),
                category: Category::PowerLossClient,
                binding: "forklift".to_string(),
                grade: graded.grade,
                recoverability: graded.recoverability.clone(),
                note: Some(notes.join("; ")),
            });
        }
    }
    if all.iter().all(|scenario| scenario.not_run.is_some()) {
        missing.push("no scenario's power loss ran".to_string());
    }
    let mut completeness = vec![
        Completeness { check: "power-loss states".to_string(), cell: Cell::Run, missing },
        Completeness { check: "power-loss soundness".to_string(), cell: Cell::Run, missing: refusals },
        Completeness { check: "power-loss states ran".to_string(), cell: Cell::Run, missing: errors },
    ];
    completeness.extend(not_run);
    let cell = if rows.is_empty() { Cell::not_run("no power-loss state was graded") } else { Cell::Run };
    Summary { cell, rows, completeness }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Root {
        Root { given: "/s".into(), canonical: "/s".into() }
    }

    /// The area before any line: a working folder holding `a`, written and synced.
    fn fixture() -> Image {
        Image::from([
            (String::new(), Entry::Dir { mode: 0o755 }),
            ("w".into(), Entry::Dir { mode: 0o755 }),
            ("w/a".into(), Entry::File { data: b"old".to_vec(), mode: 0o644 }),
        ])
    }

    /// strace's dump rows for `bytes`.
    fn dump(bytes: &[u8]) -> String {
        bytes.chunks(16).enumerate().map(|(row, chunk)| {
            let mut hex = String::new();
            for (index, byte) in chunk.iter().enumerate() {
                hex.push_str(&format!("{:02x} ", byte));
                if index == 7 {
                    hex.push(' ');
                }
            }
            let text: String = chunk.iter().map(|byte| if byte.is_ascii_graphic() { *byte as char } else { '.' }).collect();
            format!(" | {:05x}  {:<49} {:<16} |\n", row * 16, hex, text)
        }).collect()
    }

    /// A write of `bytes` through descriptor `fd` on `path`, with its dump.
    fn write(tid: u32, fd: u32, path: &str, bytes: &[u8]) -> String {
        format!("{} write({}<{}>, \"{}\", {}) = {}\n{}", tid, fd, path, String::from_utf8_lossy(bytes), bytes.len(), bytes.len(), dump(bytes))
    }

    /// The model after replaying `lines`, each a trace's text, from /s/w.
    fn replayed(lines: &[String]) -> Result<Model, String> {
        let mut replay = Replay::new(Model::of(&fixture())?, root(), 0o022);
        for line in lines {
            replay.line(&trace::parse(line)?, "/s/w", |_, _| {})?;
        }
        Ok(replay.model)
    }

    fn data(image: &Image, path: &str) -> Option<Vec<u8>> {
        match image.get(path)? {
            Entry::File { data, .. } => Some(data.clone()),
            _ => None,
        }
    }

    fn rewrite(sync: bool) -> String {
        format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_TRUNC) = 3</s/w/a>\n{}{}1 close(3</s/w/a>) = 0\n",
            write(1, 3, "/s/w/a", b"new"), if sync { "1 fsync(3</s/w/a>) = 0\n" } else { "" })
    }

    /// A write with no fsync comes back reverted, though the run left it.
    ///
    /// Revert: crash into the volatile state (a replay that kept everything) → `new` comes back.
    #[test]
    fn an_unsynced_write_comes_back_reverted() {
        let model = replayed(&[rewrite(false)]).unwrap();
        assert_eq!(data(&model.volatile_image(), "w/a").unwrap(), b"new");
        assert_eq!(data(&model.durable_image(), "w/a").unwrap(), b"old");
    }

    /// The same write with its fsync comes back kept.
    ///
    /// Revert: promote nothing on fsync (a replay that lost everything) → `old` comes back.
    #[test]
    fn a_synced_write_comes_back_kept() {
        let model = replayed(&[rewrite(true)]).unwrap();
        assert_eq!(data(&model.durable_image(), "w/a").unwrap(), b"new");
    }

    /// A file synced inside a nested new folder, the folder synced too, comes back absent while
    /// the folder above it was never synced, and present once it is.
    ///
    /// Revert: let a folder's fsync make the folder itself reachable (one that ignored nesting) →
    /// `f` comes back before `n` is synced.
    #[test]
    fn a_file_synced_under_a_new_folder_never_synced_comes_back_absent() {
        let made = format!("1 mkdir(\"n\", 0755) = 0\n\
            1 mkdirat(AT_FDCWD</s/w>, \"n/m\", 0755) = 0\n\
            1 openat(AT_FDCWD</s/w>, \"n/m/f\", O_WRONLY|O_CREAT|O_EXCL, 0644) = 3</s/w/n/m/f>\n{}\
            1 fsync(3</s/w/n/m/f>) = 0\n\
            1 openat(AT_FDCWD</s/w>, \"n/m\", O_RDONLY|O_DIRECTORY) = 4</s/w/n/m>\n\
            1 fsync(4</s/w/n/m>) = 0\n\
            1 openat(AT_FDCWD</s/w>, \".\", O_RDONLY|O_DIRECTORY) = 5</s/w>\n\
            1 fsync(5</s/w>) = 0\n", write(1, 3, "/s/w/n/m/f", b"f"));
        let durable = replayed(std::slice::from_ref(&made)).unwrap().durable_image();
        assert!(durable.contains_key("w/n"), "n's own entry was synced");
        assert!(!durable.contains_key("w/n/m"), "m's entry in n never was");
        assert!(!durable.contains_key("w/n/m/f"));
        let synced = "1 openat(AT_FDCWD</s/w>, \"n\", O_RDONLY|O_DIRECTORY) = 3</s/w/n>\n1 fsync(3</s/w/n>) = 0\n".to_string();
        let durable = replayed(&[made, synced]).unwrap().durable_image();
        assert_eq!(data(&durable, "w/n/m/f").unwrap(), b"f");
    }

    /// A file one line made and synced but whose entry it never synced is absent after a second
    /// line that syncs a file naming it, and present once a third syncs their folder.
    ///
    /// Revert: take everything as durable when a line starts (a replay that reset per command) →
    /// `obj` comes back after the second line.
    #[test]
    fn a_file_one_line_left_unsynced_is_absent_after_a_second_that_syncs() {
        let first = format!("1 openat(AT_FDCWD</s/w>, \"obj\", O_WRONLY|O_CREAT|O_EXCL, 0644) = 3</s/w/obj>\n{}1 fsync(3</s/w/obj>) = 0\n",
            write(1, 3, "/s/w/obj", b"o"));
        let second = format!("2 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_TRUNC) = 3</s/w/a>\n{}2 fsync(3</s/w/a>) = 0\n",
            write(2, 3, "/s/w/a", b"obj"));
        let durable = replayed(&[first.clone(), second.clone()]).unwrap().durable_image();
        assert_eq!(data(&durable, "w/a").unwrap(), b"obj");
        assert!(!durable.contains_key("w/obj"));
        let third = "3 openat(AT_FDCWD</s/w>, \".\", O_RDONLY|O_DIRECTORY) = 4</s/w>\n3 fsync(4</s/w>) = 0\n".to_string();
        let durable = replayed(&[first, second, third]).unwrap().durable_image();
        assert_eq!(data(&durable, "w/obj").unwrap(), b"o", "a folder's fsync promotes an earlier line's entries");
    }

    /// A temp written, synced and renamed over an existing name comes back with the new contents
    /// once its folder is synced after the rename, and with the old ones, and no temp, before.
    ///
    /// Revert: keep the replaced name's inode and copy the moved contents into it (a replay keyed
    /// by path) → `old` comes back after the folder's fsync.
    #[test]
    fn a_temp_renamed_over_a_name_is_new_only_once_its_folder_is_synced() {
        let renamed = format!("1 openat(AT_FDCWD</s/w>, \"a.tmp\", O_WRONLY|O_CREAT|O_EXCL, 0644) = 3</s/w/a.tmp>\n{}\
            1 fsync(3</s/w/a.tmp>) = 0\n1 close(3</s/w/a.tmp>) = 0\n\
            1 renameat2(AT_FDCWD</s/w>, \"a.tmp\", AT_FDCWD</s/w>, \"a\", 0) = 0\n", write(1, 3, "/s/w/a.tmp", b"new"));
        let durable = replayed(std::slice::from_ref(&renamed)).unwrap().durable_image();
        assert_eq!(data(&durable, "w/a").unwrap(), b"old");
        assert!(!durable.contains_key("w/a.tmp"));
        let synced = format!("{}1 openat(AT_FDCWD</s/w>, \".\", O_RDONLY|O_DIRECTORY) = 4</s/w>\n1 fsync(4</s/w>) = 0\n", renamed);
        let durable = replayed(&[synced]).unwrap().durable_image();
        assert_eq!(data(&durable, "w/a").unwrap(), b"new");
        assert!(!durable.contains_key("w/a.tmp"));
    }

    /// A forked child writes through the descriptors it inherited, whichever of its calls and its
    /// parent's clone prints first; offsets advance, appends land at the end, and a positioned
    /// write leaves the offset alone.
    ///
    /// Revert: give every process an empty table → the child's write refuses as never opened.
    #[test]
    fn a_child_writes_through_what_it_inherited() {
        let line = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_APPEND) = 3</s/w/a>\n\
            {}1 clone(child_stack=NULL, flags=CLONE_CHILD_SETTID|SIGCHLD) = 2\n{}\
            1 openat(AT_FDCWD</s/w>, \"b\", O_RDWR|O_CREAT, 0755) = 4</s/w/b>\n{}\
            1 pwrite64(4</s/w/b>, \"Z\", 1, 0) = 1\n{}{}",
            write(2, 3, "/s/w/a", b"+child"), write(1, 3, "/s/w/a", b"+parent"), write(1, 4, "/s/w/b", b"xy"), dump(b"Z"), write(1, 4, "/s/w/b", b"!"));
        let volatile = replayed(&[line]).unwrap().volatile_image();
        assert_eq!(data(&volatile, "w/a").unwrap(), b"old+child+parent");
        assert_eq!(data(&volatile, "w/b").unwrap(), b"Zy!");
        assert_eq!(volatile.get("w/b"), Some(&Entry::File { data: b"Zy!".to_vec(), mode: 0o755 }));
    }

    /// What the model cannot replay refuses rather than being skipped: a write whose bytes the
    /// trace did not dump, a write through a descriptor into the area never seen opened, an
    /// unmodelled change inside the area, and an unnamed temp file. The same calls outside the
    /// area change nothing.
    ///
    /// Revert: skip unmodelled calls → the fallocate replays as nothing and the test's refusal is
    /// missing.
    #[test]
    fn what_the_model_cannot_replay_refuses() {
        let short = "1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 write(3</s/w/a>, \"new\", 3) = 3\n".to_string();
        assert!(replayed(&[short]).unwrap_err().contains("dumped 0 of its 3 bytes"));
        assert!(replayed(&[write(1, 7, "/s/w/a", b"x")]).unwrap_err().contains("never seen opened"));
        let allocated = "1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 fallocate(3</s/w/a>, 0, 0, 4096) = 0\n".to_string();
        assert!(replayed(&[allocated]).unwrap_err().contains("does not replay"));
        let unnamed = "1 openat(AT_FDCWD</s/w>, \".\", O_RDWR|O_TMPFILE, 0600) = 3</s/w/#123 (deleted)>\n".to_string();
        assert!(replayed(&[unnamed]).unwrap_err().contains("unnamed temp"));
        let outside = format!("1 openat(AT_FDCWD</s/w>, \"/t/x\", O_WRONLY|O_CREAT, 0644) = 3</t/x>\n{}1 fallocate(3</t/x>, 0, 0, 1) = 0\n1 write(1<pipe:[5]>, \"y\", 1) = 1\n", write(1, 3, "/t/x", b"x"));
        assert_eq!(replayed(&[outside]).unwrap().volatile_image(), fixture());
    }

    /// A sync that another thread's change overlapped refuses, since whether the sync covered
    /// that change is not known; the same change made before the sync started is covered, and
    /// one to something the sync does not cover is no overlap.
    ///
    /// Revert: replay calls where they finished, never asking where they started → the folder's
    /// new entry reads as durable.
    #[test]
    fn a_sync_another_thread_overlapped_refuses() {
        let threaded = |between: &str, before: &str| format!("1 openat(AT_FDCWD</s/w>, \".\", O_RDONLY|O_DIRECTORY) = 3</s/w>\n\
            1 clone(child_stack=0x1, flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM) = 2\n\
            {}2 fsync(3</s/w> <unfinished ...>\n{}2 <... fsync resumed>) = 0\n", before, between);
        let made = "1 mkdir(\"n\", 0755) = 0\n";
        assert!(replayed(&[threaded(made, "")]).unwrap_err().contains("overlapped"));
        let model = replayed(&[threaded("", made)]).unwrap();
        assert!(model.durable_image().contains_key("w/n"), "made before the sync started");
        let elsewhere = "1 mkdir(\"/s/m\", 0755) = 0\n";
        assert!(replayed(&[threaded(elsewhere, "")]).is_ok(), "a change the sync does not cover");
    }

    /// A descriptor number strace names as something outside the area is no longer the area
    /// file the table remembers under it, and an open whose descriptor strace did not name
    /// refuses.
    ///
    /// Revert: trust the table over strace's name → the pipe's byte lands in `a`; skip an unnamed
    /// open → it replays as nothing.
    #[test]
    fn a_reused_descriptor_follows_what_strace_names() {
        let reused = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 write(3<pipe:[9]>, \"x\", 1) = 1\n{}", dump(b"x"));
        assert_eq!(data(&replayed(&[reused]).unwrap().volatile_image(), "w/a").unwrap(), b"old");
        let unnamed = "1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3\n".to_string();
        assert!(replayed(&[unnamed]).unwrap_err().contains("did not name"));
    }

    /// A close another thread's open overlapped frees only the descriptor it closed: the number
    /// the open was handed while the close ran is the new file, and writes through it land.
    ///
    /// Revert: let a close remove whatever its number holds when it finishes → the write refuses
    /// as a descriptor never seen opened.
    #[test]
    fn an_overlapped_close_leaves_the_descriptor_reopened_under_its_number() {
        let text = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_RDONLY) = 3</s/w/a>\n\
            1 clone(child_stack=0x1, flags=CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM) = 2\n\
            1 close(3</s/w/a> <unfinished ...>\n\
            2 openat(AT_FDCWD</s/w>, \"b\", O_WRONLY|O_CREAT, 0644) = 3</s/w/b>\n\
            1 <... close resumed>) = 0\n{}", write(2, 3, "/s/w/b", b"x"));
        let model = replayed(&[text]).unwrap();
        assert_eq!(data(&model.volatile_image(), "w/b").unwrap(), b"x");
    }

    /// The replay is held to the run: any path where its tree and the run's differ is named.
    ///
    /// Revert: compare paths only → a file whose bytes differ passes; skip folder modes → a folder
    /// made under another umask passes.
    #[test]
    fn a_replay_that_does_not_reproduce_the_run_is_named() {
        let replayed = replayed(&[rewrite(false)]).unwrap().volatile_image();
        let mut ran = fixture();
        ran.insert("w/a".into(), Entry::File { data: b"new".to_vec(), mode: 0o644 });
        assert_eq!(difference(&replayed, &ran), None);
        ran.insert("w/a".into(), Entry::File { data: b"neW".to_vec(), mode: 0o644 });
        assert!(difference(&replayed, &ran).unwrap().starts_with("w/a:"));
        ran.insert("w/a".into(), Entry::File { data: b"new".to_vec(), mode: 0o755 });
        assert!(difference(&replayed, &ran).unwrap().contains("mode 644 and the run left 755"));
        ran.insert("w/a".into(), Entry::File { data: b"new".to_vec(), mode: 0o600 });
        assert!(difference(&replayed, &ran).unwrap().contains("mode 644 and the run left 600"));
        ran.insert("w/a".into(), Entry::File { data: b"new".to_vec(), mode: 0o644 });
        ran.insert("w".into(), Entry::Dir { mode: 0o700 });
        assert_eq!(difference(&replayed, &ran).as_deref(), Some("w: the replay makes it mode 755 and the run left 700"));
        ran.insert("w".into(), Entry::Dir { mode: 0o755 });
        ran.insert("w/a".into(), Entry::File { data: b"new".to_vec(), mode: 0o644 });
        ran.insert("w/z".into(), Entry::Link { target: "a".into() });
        assert!(difference(&replayed, &ran).unwrap().starts_with("w/z:"));
    }

    /// Crashes land before each kill point's call and after each line; a state many moments leave
    /// is graded once, counted; only the moment after the last line is acknowledged, even when it
    /// leaves the same state as an earlier one.
    ///
    /// Revert: take the state after the call → the crash at the fsync already holds `new`; key a
    /// state by its tree alone → the last line's state merges into an earlier unacknowledged one.
    #[test]
    fn crashes_land_before_each_point_and_after_each_line() {
        let texts = [rewrite(true), "2 openat(AT_FDCWD</s/w>, \"a\", O_RDONLY) = 3</s/w/a>\n".to_string()];
        let calls: Vec<Vec<Call>> = texts.iter().map(|text| trace::parse(text).unwrap()).collect();
        let points: Vec<Vec<Point>> = calls.iter().enumerate()
            .map(|(line, calls)| kill::enumerate(calls, line, "stack", &root(), "/s/w", |_| false).unwrap().points).collect();
        assert_eq!(points[0].iter().map(|point| point.name.as_str()).collect::<Vec<_>>(), ["openat", "write", "fsync"]);
        let lines: Vec<Line> = calls.iter().zip(&points)
            .map(|(calls, points)| Line { calls, command: "stack".into(), points: points.iter().collect() }).collect();
        let (crashes, replayed) = crashes(&fixture(), &root(), "/s/w", 0o022, &lines, 0, 7).unwrap();
        assert_eq!(data(&replayed, "w/a").unwrap(), b"new");
        let found: Vec<(Vec<u8>, bool, usize)> = crashes.iter().map(|crash| (data(&crash.image, "w/a").unwrap(), crash.acknowledged, crash.shared)).collect();
        // Before the open, the write and the fsync: old. After line 1: new. After line 2: new, acknowledged.
        assert_eq!(found, vec![(b"old".to_vec(), false, 3), (b"new".to_vec(), false, 1), (b"new".to_vec(), true, 1)]);
        assert!(matches!(&crashes[0].moment, Moment::During { point } if point.name == "openat"));
        assert_eq!(crashes[2].moment, Moment::After { line: 1, command: "stack".into() });
    }

    /// A child's syncs are crash moments though no kill point stands at them: the state its first
    /// fsync made durable, before its second, is graded.
    ///
    /// Revert: record crashes at kill points only → the child's `new` is never a crash state.
    #[test]
    fn a_crash_lands_before_every_sync_a_child_makes() {
        let text = format!("1 clone(child_stack=NULL, flags=CLONE_CHILD_CLEARTID|CLONE_CHILD_SETTID|SIGCHLD) = 9\n\
            9 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_TRUNC) = 3</s/w/a>\n{}9 fsync(3</s/w/a>) = 0\n{}9 fsync(3</s/w/a>) = 0\n",
            write(9, 3, "/s/w/a", b"new"), write(9, 3, "/s/w/a", b"more"));
        let calls = trace::parse(&text).unwrap();
        let points = kill::enumerate(&calls, 0, "stack", &root(), "/s/w", |_| false).unwrap().points;
        assert!(points.iter().all(|point| point.name != "fsync"), "the child's syncs are no kill points");
        let lines = [Line { calls: &calls, command: "stack".into(), points: points.iter().collect() }];
        let (crashes, _) = crashes(&fixture(), &root(), "/s/w", 0o022, &lines, 0, 7).unwrap();
        let held: Vec<Vec<u8>> = crashes.iter().map(|crash| data(&crash.image, "w/a").unwrap()).collect();
        assert!(held.contains(&b"new".to_vec()), "{:?}", held);
        assert!(crashes.iter().any(|crash| matches!(&crash.moment, Moment::Synced { name, .. } if name == "fsync")));
    }

    /// A crash state that records the counting area's root is found, in a file's bytes or a
    /// link's target, at a path-component boundary only.
    ///
    /// Revert: check only the run's final tree → a lock naming the area mid-line is graded.
    #[test]
    fn a_crash_state_naming_the_counting_root_is_found() {
        let mut image = fixture();
        assert_eq!(bound(&image, &["/s"]), None);
        image.insert("w/lock".into(), Entry::File { data: b"held by /s/w".to_vec(), mode: 0o644 });
        assert_eq!(bound(&image, &["/s"]).as_deref(), Some("w/lock"));
        image.insert("w/lock".into(), Entry::File { data: b"held by /sx".to_vec(), mode: 0o644 });
        image.insert("w/z".into(), Entry::Link { target: "/s/w/a".into() });
        assert_eq!(bound(&image, &["/s"]).as_deref(), Some("w/z"));
    }

    /// A file cut short and rewritten since its sync can come back as any prefix of its new
    /// contents, empty included, and never as its old bytes with the new ones' tail; one only
    /// appended to keeps its synced bytes.
    ///
    /// Revert: tear every file by growing its synced contents → `old` with `newer`'s tail comes
    /// back, and the empty file never does.
    #[test]
    fn a_truncated_file_tears_to_a_prefix_of_its_new_contents() {
        let rewritten = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_TRUNC) = 3</s/w/a>\n{}", write(1, 3, "/s/w/a", b"newer"));
        let model = replayed(&[rewritten]).unwrap();
        let mut rng = Rng::new(3);
        let torn: BTreeSet<Vec<u8>> = (0..200).map(|_| data(&model.variant(&mut rng), "w/a").unwrap()).collect();
        assert!(torn.contains(&Vec::new()), "{:?}", torn);
        assert!(torn.iter().all(|held| held == b"old" || b"newer".starts_with(held)), "{:?}", torn);
        let appended = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_APPEND) = 3</s/w/a>\n{}", write(1, 3, "/s/w/a", b"er"));
        let model = replayed(&[appended]).unwrap();
        let mut rng = Rng::new(3);
        assert!((0..200).all(|_| data(&model.variant(&mut rng), "w/a").unwrap().starts_with(b"old")));
    }

    /// The crash states stop before grading: refused when the replay does not reproduce the run,
    /// which outranks a state that records the counting root, which stops them as not run.
    ///
    /// Revert: check the states for the root first → the unreproduced replay reads as not run,
    /// which fails nothing.
    #[test]
    fn an_unreproduced_replay_outranks_a_path_bound_state() {
        let mut held = fixture();
        held.insert("w/lock".into(), Entry::File { data: b"/s/w".to_vec(), mode: 0o644 });
        let moment = Moment::After { line: 0, command: "stack".into() };
        let crashes = [Crash { moment, acknowledged: false, shared: 1, variant: None, image: std::sync::Arc::new(held) }];
        assert_eq!(stopped(&fixture(), &fixture(), &[], &["/s"]), None);
        let (refusals, not_run) = stopped(&fixture(), &fixture(), &crashes, &["/s"]).unwrap();
        assert!(refusals.is_empty() && not_run.unwrap().contains("w/lock"));
        let mut ran = fixture();
        ran.remove("w/a");
        let (refusals, not_run) = stopped(&fixture(), &ran, &crashes, &["/s"]).unwrap();
        assert!(refusals[0].contains("does not reproduce") && not_run.is_none(), "{:?}", refusals);
    }

    /// A name that is not UTF-8 stops the scan rather than read as another name.
    ///
    /// Revert: read names lossily → the scan holds a name with a replacement character.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_name_that_is_not_utf8_stops_the_scan() {
        use std::os::unix::ffi::OsStrExt;
        let area = std::env::temp_dir().join(format!("proof-load-scan-{}", std::process::id()));
        std::fs::create_dir_all(&area).unwrap();
        std::fs::write(area.join(std::ffi::OsStr::from_bytes(b"n\xff")), b"x").unwrap();
        let scanned = scan(&area);
        let _ = remove_tree(&area);
        assert!(scanned.unwrap_err().to_string().contains("not UTF-8"));
    }

    /// A dump row strace printed that cannot be read leaves the trace readable, for the kill
    /// points, and refuses only a replay of that write's bytes.
    ///
    /// Revert: refuse the trace on the row → the kill points of the line are lost with it.
    #[test]
    fn an_unreadable_dump_refuses_only_the_replay_of_its_bytes() {
        let text = "1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 write(3</s/w/a>, \"x\", 1) = 1\n | zz\n".to_string();
        let calls = trace::parse(&text).unwrap();
        assert!(calls[1].unreadable.as_deref().unwrap().contains("an unreadable dump row"));
        assert!(replayed(&[text]).unwrap_err().contains("an unreadable dump row"));
    }

    /// A crash moment's label names its line, so two lines' moments at the same call read apart.
    ///
    /// Revert: leave the line out → both read `stack before fsync at traced call 4`.
    #[test]
    fn a_moment_label_names_its_line() {
        let moment = |line| Moment::Synced { line, command: "stack".into(), call: 3, name: "fsync".into() };
        assert_eq!(moment(1).label(), "line 2 stack before fsync at traced call 4");
        assert_ne!(moment(0).label(), moment(1).label());
    }

    /// A close_range that unshares first closes in the caller's own copy: the thread it shared
    /// the table with keeps its descriptors.
    ///
    /// Revert: close in the shared table → the other thread's write refuses as never opened.
    #[test]
    fn an_unsharing_close_range_leaves_the_shared_table_whole() {
        let text = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n\
            1 clone(child_stack=0x1, flags=CLONE_VM|CLONE_FS|CLONE_FILES|SIGCHLD) = 9\n\
            9 close_range(3, 4294967295, CLOSE_RANGE_UNSHARE) = 0\n{}", write(1, 3, "/s/w/a", b"x"));
        assert_eq!(data(&replayed(&[text]).unwrap().volatile_image(), "w/a").unwrap(), b"xld");
    }

    /// An fdatasync makes a file's bytes durable and not its mode; an fsync makes both.
    ///
    /// Revert: promote the whole node on fdatasync → the crash keeps the new mode.
    #[test]
    fn an_fdatasync_keeps_the_synced_mode() {
        let synced = |call: &str| format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 fchmod(3</s/w/a>, 0600) = 0\n{}1 {}(3</s/w/a>) = 0\n", write(1, 3, "/s/w/a", b"new"), call);
        let durable = replayed(&[synced("fdatasync")]).unwrap().durable_image();
        assert_eq!(durable.get("w/a"), Some(&Entry::File { data: b"new".to_vec(), mode: 0o644 }));
        let durable = replayed(&[synced("fsync")]).unwrap().durable_image();
        assert_eq!(durable.get("w/a"), Some(&Entry::File { data: b"new".to_vec(), mode: 0o600 }));
    }

    /// A file overwritten in place without growing can come back torn there: a prefix of the new
    /// bytes over the synced ones.
    ///
    /// Revert: keep the synced contents of a file that did not grow → only `old` and `new` come
    /// back.
    #[test]
    fn an_overwrite_in_place_tears_in_place() {
        let overwritten = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n{}", write(1, 3, "/s/w/a", b"new"));
        let model = replayed(&[overwritten]).unwrap();
        let mut rng = Rng::new(5);
        let torn: BTreeSet<Vec<u8>> = (0..200).map(|_| data(&model.variant(&mut rng), "w/a").unwrap()).collect();
        assert!(torn.iter().any(|held| held != b"old" && held != b"new"), "{:?}", torn);
        assert!(torn.iter().all(|held| (0..=3).any(|landed| held[..landed] == b"new"[..landed] && held[landed..] == b"old"[landed..])), "{:?}", torn);
    }

    /// Files and folders are made under the umask the command started under, or the one it set.
    ///
    /// Revert: assume 022 → the file comes back 0644 under 077.
    #[test]
    fn files_are_made_under_the_umask_in_force() {
        let made = "1 openat(AT_FDCWD</s/w>, \"n\", O_WRONLY|O_CREAT, 0666) = 3</s/w/n>\n1 mkdir(\"d\", 0777) = 0\n";
        let mut replay = Replay::new(Model::of(&fixture()).unwrap(), root(), 0o077);
        replay.line(&trace::parse(made).unwrap(), "/s/w", |_, _| {}).unwrap();
        let image = replay.model.volatile_image();
        assert_eq!(image.get("w/n"), Some(&Entry::File { data: Vec::new(), mode: 0o600 }));
        assert_eq!(image.get("w/d"), Some(&Entry::Dir { mode: 0o700 }));
        let set = format!("1 umask(027) = 022\n{}", made);
        let image = replayed(&[set]).unwrap().volatile_image();
        assert_eq!(image.get("w/n"), Some(&Entry::File { data: Vec::new(), mode: 0o640 }));
        assert_eq!(image.get("w/d"), Some(&Entry::Dir { mode: 0o750 }));
    }

    /// A syncfs promises only its own file system: one through a descriptor outside the area makes
    /// nothing in it durable, and one inside makes everything durable.
    ///
    /// Revert: treat every syncfs as sync → the unsynced write reads as durable.
    #[test]
    fn a_syncfs_outside_the_area_promises_it_nothing() {
        let synced = |fd: &str| format!("{}1 openat(AT_FDCWD</s/w>, \"{}\", O_RDONLY) = 4<{}>\n1 syncfs(4<{}>) = 0\n", rewrite(false), fd, fd, fd);
        assert_eq!(data(&replayed(&[synced("/t/x")]).unwrap().durable_image(), "w/a").unwrap(), b"old");
        assert_eq!(data(&replayed(&[synced("/s/w/a")]).unwrap().durable_image(), "w/a").unwrap(), b"new");
    }

    /// A failed close frees its number all the same, and a descriptor strace names as another file
    /// in the area than the one the table holds refuses rather than write the wrong file.
    ///
    /// Revert: skip failed calls → the write lands in `a` through the closed number; trust the
    /// table → the byte meant for `b` lands in `a`.
    #[test]
    fn a_descriptor_is_held_only_while_it_names_its_file() {
        let closed = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n1 close(3</s/w/a>) = -1 EINTR (Interrupted system call)\n{}", write(1, 3, "/s/w/a", b"x"));
        assert!(replayed(&[closed]).unwrap_err().contains("never seen opened"));
        let other = format!("1 openat(AT_FDCWD</s/w>, \"b\", O_WRONLY|O_CREAT, 0644) = 4</s/w/b>\n1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n{}", write(1, 3, "/s/w/b", b"x"));
        assert!(replayed(&[other]).unwrap_err().contains("not the file the replay saw opened"));
    }

    /// A crash is graded with the trail it happens on: a state left after a line finished and
    /// again inside the next line, before it changed anything durable, is two crashes, since the
    /// second is graded with that line cut short.
    ///
    /// Revert: key a state by its tree alone → the next line's kill points merge into the crash
    /// after the first line.
    #[test]
    fn one_state_on_two_trails_is_two_crashes() {
        let texts = [rewrite(false), rewrite(false)];
        let calls: Vec<Vec<Call>> = texts.iter().map(|text| trace::parse(text).unwrap()).collect();
        let points: Vec<Vec<Point>> = calls.iter().enumerate()
            .map(|(line, calls)| kill::enumerate(calls, line, "stack", &root(), "/s/w", |_| true).unwrap().points).collect();
        let lines: Vec<Line> = calls.iter().zip(&points)
            .map(|(calls, points)| Line { calls, command: "stack".into(), points: points.iter().collect() }).collect();
        let (crashes, _) = crashes(&fixture(), &root(), "/s/w", 0o022, &lines, 0, 7).unwrap();
        assert!(crashes.iter().all(|crash| data(&crash.image, "w/a").unwrap() == b"old"));
        let trails: Vec<(usize, bool)> = crashes.iter().map(|crash| crash.moment.trail()).collect();
        assert_eq!(trails, [(0, true), (1, false), (1, true), (2, false)]);
    }

    /// A durable state is never reported as a variant: drawing variants leaves the durable
    /// crashes exactly as a run with none, even when a draw happens to match a later one.
    ///
    /// Revert: count a durable state into the variant it matches → it is missing from the
    /// durable crashes for some seed.
    #[test]
    fn a_durable_state_a_variant_found_first_stays_durable() {
        // The fsync makes `new` durable; the mkdir after it leaves that state at its own point, so
        // a variant drawn at the fsync that takes `new` matches it.
        let text = format!("{}1 mkdir(\"n\", 0755) = 0\n", rewrite(true).replace("1 close(3</s/w/a>) = 0\n", ""));
        let calls = trace::parse(&text).unwrap();
        let points = kill::enumerate(&calls, 0, "stack", &root(), "/s/w", |_| true).unwrap().points;
        let lines = [Line { calls: &calls, command: "stack".into(), points: points.iter().collect() }];
        let durable = |crashes: &[Crash]| -> Vec<(String, usize)> {
            crashes.iter().filter(|crash| crash.variant.is_none()).map(|crash| (crash.moment.label(), crash.shared)).collect()
        };
        let (plain, _) = crashes(&fixture(), &root(), "/s/w", 0o022, &lines, 0, 0).unwrap();
        let mut matched = false;
        for seed in 0..64 {
            let (drawn, _) = crashes(&fixture(), &root(), "/s/w", 0o022, &lines, 8, seed).unwrap();
            assert_eq!(durable(&drawn), durable(&plain), "seed {}", seed);
            matched |= drawn.len() < plain.len() + 8;
        }
        assert!(matched, "some seed draws a variant that matches a durable state");
    }

    /// pwritev2 at offset -1 writes at the description's offset, RWF_APPEND appends, an O_APPEND
    /// set by fcntl after the open makes later writes append, and so does a positioned write on
    /// an O_APPEND description, as Linux does.
    ///
    /// Revert: read pwritev2's -1 as an offset → the replay refuses; ignore F_SETFL → `+x`
    /// overwrites from the start; honour pwrite64's offset under O_APPEND → `?` lands at 0.
    #[test]
    fn positioned_and_appending_writes_land_where_the_kernel_puts_them() {
        let vectored = |fd: u32, offset: &str, flags: &str, bytes: &[u8]| format!(
            "1 pwritev2({}</s/w/a>, [{{iov_base=\"{}\", iov_len={}}}], 1, {}, {}) = {}\n{}",
            fd, String::from_utf8_lossy(bytes), bytes.len(), offset, flags, bytes.len(), dump(bytes));
        let text = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 4</s/w/a>\n{}{}\
            1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY) = 3</s/w/a>\n\
            1 fcntl(3</s/w/a>, F_SETFL, O_WRONLY|O_APPEND) = 0\n{}{}",
            vectored(4, "-1", "0", b"NE"), write(1, 4, "/s/w/a", b"W"), write(1, 3, "/s/w/a", b"+x"), vectored(4, "0", "RWF_APPEND", b"!"));
        let positioned = format!("1 pwrite64(3</s/w/a>, \"?\", 1, 0) = 1\n{}", dump(b"?"));
        let model = replayed(&[format!("{}{}", text, positioned)]).unwrap();
        assert_eq!(data(&model.volatile_image(), "w/a").unwrap(), b"NEW+x!?");
    }

    /// Seeded variants lie between the durable and the volatile state: each entry and file from
    /// one or the other, or a file grown past its synced size with a zero or torn tail. One seed
    /// draws the same variants every time.
    ///
    /// Revert: draw from an unseeded source → two runs of one seed differ.
    #[test]
    fn seeded_variants_lie_between_the_durable_and_the_volatile_state() {
        let line = format!("1 openat(AT_FDCWD</s/w>, \"a\", O_WRONLY|O_APPEND) = 3</s/w/a>\n{}\
            1 openat(AT_FDCWD</s/w>, \"b\", O_WRONLY|O_CREAT, 0644) = 4</s/w/b>\n", write(1, 3, "/s/w/a", b"-appended"));
        let model = replayed(&[line]).unwrap();
        let mut seen = BTreeSet::new();
        let mut rng = Rng::new(3);
        for _ in 0..64 {
            let variant = model.variant(&mut rng);
            let a = data(&variant, "w/a").unwrap();
            assert!(a.starts_with(b"old") && a.len() <= b"old-appended".len());
            let tail = &a[3..];
            assert!(tail.iter().all(|byte| *byte == 0) || b"-appended".starts_with(tail), "{:?}", a);
            assert!(variant.keys().all(|path| ["", "w", "w/a", "w/b"].contains(&path.as_str())));
            seen.insert((a, variant.contains_key("w/b")));
        }
        assert!(seen.len() > 3, "variants differ: {:?}", seen);
        let draw = |seed| { let mut rng = Rng::new(seed); (0..8).map(|_| model.variant(&mut rng)).collect::<Vec<_>>() };
        assert_eq!(draw(5), draw(5));
    }

    /// A tree written out reads back as itself, and is removed whole, its read-only folder too.
    ///
    /// Revert: skip the links → the scan misses `w/l`; remove without making the folders writable
    /// → the read-only folder's file cannot be removed.
    #[cfg(unix)]
    #[test]
    fn a_materialized_tree_scans_back_as_itself() {
        let scratch = std::env::temp_dir().join(format!("proof-load-power-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("stale"), b"x").unwrap();
        let mut image = fixture();
        image.insert(String::new(), Entry::Dir { mode: 0o700 });
        image.insert("w/l".into(), Entry::Link { target: "a".into() });
        image.insert("w/x".into(), Entry::File { data: b"#!".to_vec(), mode: 0o755 });
        image.insert("r".into(), Entry::Dir { mode: 0o555 });
        image.insert("r/in".into(), Entry::File { data: vec![], mode: 0o444 });
        materialize(&image, &scratch).unwrap();
        // Written again over its own read-only folder, as a second state over one copy would be.
        materialize(&image, &scratch).unwrap();
        let scanned = scan(&scratch);
        remove_tree(&scratch).unwrap();
        assert_eq!(scanned.unwrap(), image);
        assert!(std::fs::symlink_metadata(&scratch).is_err(), "the read-only folder is removed too");
    }
}
