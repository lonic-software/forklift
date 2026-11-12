//! Reading what strace saw: the trace of one command run under `strace -f -qq -y -o <file>`, its
//! calls classified against three closed classes, and the paths each one touched.
//!
//! The classes are mutating, syncing, and known non-mutating. A call in none of them refuses the
//! run as unmodelled, and is resolved by adding its name to a class here, because a write the
//! table does not know about is a kill point the harness would silently never exercise. So does a
//! shared writable file mapping or io_uring, since a write through either reaches the file without
//! a call strace sees.

use std::collections::BTreeMap;

/// What a call returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Returned {
    /// A value, as strace printed it (a descriptor keeps its `-y` path: `3</w/a>`).
    Value(String),
    /// `-1` with this errno name.
    Failed(String),
    /// `= ?`: the call never returned, because its process was killed in it.
    Unknown,
    /// `= ? ERESTART…`: interrupted by a signal and restarted, so it returns later on its own line.
    Restarted,
}

/// One call, its unfinished and resumed halves joined.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The thread that made it.
    pub tid: u32,
    pub name: String,
    /// Each argument's text as strace printed it.
    pub arguments: Vec<String>,
    pub returned: Returned,
}

/// One argument read as what it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Argument {
    /// A quoted string, decoded.
    Text(String),
    /// A descriptor and the path `-y` printed for it: a file path, or `pipe:[…]`, `socket:[…]`,
    /// `anon_inode:[…]` for what has none.
    Descriptor { fd: i64, target: String },
    /// `AT_FDCWD`, with the working directory `-y` printed for it when it printed one.
    Cwd(Option<String>),
    /// Anything else: flags, numbers, structures.
    Other(String),
}

/// Reads `text`, the output of one `strace -f -qq -y -o` run. A line the harness cannot read
/// refuses the trace: a guess at a call is a kill point aimed at nothing.
pub fn parse(text: &str) -> Result<Vec<Call>, String> {
    let mut calls = Vec::new();
    // Per thread, the first half of an unfinished call.
    let mut pending: BTreeMap<u32, String> = BTreeMap::new();
    for (number, line) in text.lines().enumerate() {
        let refuse = |why: &str| format!("trace line {}: {}: {}", number + 1, why, line);
        if line.trim().is_empty() {
            continue;
        }
        let (tid, rest) = line.trim_start().split_once(char::is_whitespace).ok_or_else(|| refuse("no thread id"))?;
        let tid: u32 = tid.parse().map_err(|_| refuse("no thread id"))?;
        let rest = rest.trim_start();
        if rest.starts_with("+++") || rest.starts_with("---") {
            continue;
        }
        let whole = if let Some(resumed) = rest.strip_prefix("<... ") {
            let (_, tail) = resumed.split_once(" resumed>").ok_or_else(|| refuse("a resumed call without its name"))?;
            let head = pending.remove(&tid).ok_or_else(|| refuse("a resumed call that never started"))?;
            format!("{}{}", head, tail)
        } else {
            rest.to_string()
        };
        if let Some(head) = whole.strip_suffix("<unfinished ...>") {
            pending.insert(tid, head.trim_end().to_string());
            continue;
        }
        calls.push(call(tid, &whole).ok_or_else(|| refuse("not a call"))?);
    }
    // A call still unfinished at the end never returned: its process died in it.
    for (tid, head) in pending {
        if let Some(mut started) = call(tid, &format!("{}) = ?", head)) {
            started.returned = Returned::Unknown;
            calls.push(started);
        }
    }
    Ok(calls)
}

/// One whole call line: `name(arguments) = returned`.
fn call(tid: u32, text: &str) -> Option<Call> {
    let open = text.find('(')?;
    let name = &text[..open];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let (arguments, after) = split_arguments(&text[open + 1..])?;
    let returned = after.trim_start().strip_prefix('=')?.trim();
    let returned = if let Some(rest) = returned.strip_prefix('?') {
        if rest.trim_start().starts_with("ERESTART") { Returned::Restarted } else { Returned::Unknown }
    } else if let Some(rest) = returned.strip_prefix("-1 ") {
        Returned::Failed(rest.split_whitespace().next().unwrap_or_default().to_string())
    } else {
        Returned::Value(value_token(returned))
    };
    Some(Call { tid, name: name.to_string(), arguments, returned })
}

/// The leading value of a return, with its `-y` annotation when it has one.
fn value_token(text: &str) -> String {
    let mut end = text.find(char::is_whitespace).unwrap_or(text.len());
    if let Some(open) = text.find('<').filter(|open| *open < end) {
        if let Some(close) = text[open..].find('>') {
            end = open + close + 1;
        }
    }
    text[..end].to_string()
}

/// Splits the text after a call's `(` into its top-level arguments, up to the matching `)`, and
/// returns what follows it. Quotes, brackets, braces and `-y` annotations nest.
fn split_arguments(text: &str) -> Option<(Vec<String>, &str)> {
    let bytes = text.as_bytes();
    let mut arguments = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
            }
            // A `-y` annotation follows a descriptor's number or AT_FDCWD, and runs to its `>`.
            b'<' if index > 0 && (bytes[index - 1].is_ascii_digit() || text[..index].ends_with("AT_FDCWD")) => {
                index += text[index..].find('>')?;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b']' | b'}' => depth = depth.saturating_sub(1),
            b')' if depth == 0 => {
                let last = text[start..index].trim();
                if !last.is_empty() || !arguments.is_empty() {
                    arguments.push(last.to_string());
                }
                return Some((arguments, &text[index + 1..]));
            }
            b')' => depth -= 1,
            b',' if depth == 0 => {
                arguments.push(text[start..index].trim().to_string());
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// What one argument's text names.
pub fn argument(text: &str) -> Argument {
    if let Some(quoted) = text.strip_prefix('"') {
        return Argument::Text(unquote(quoted));
    }
    if let Some(rest) = text.strip_prefix("AT_FDCWD") {
        return Argument::Cwd(annotation(rest));
    }
    let digits = text.trim_start_matches('-');
    let number_end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    if number_end > 0 {
        if let Some(target) = annotation(&digits[number_end..]) {
            if let Ok(fd) = text[..text.len() - digits.len() + number_end].parse() {
                return Argument::Descriptor { fd, target };
            }
        }
    }
    Argument::Other(text.to_string())
}

/// The path inside a `<…>` annotation, with the ` (deleted)` mark of an unlinked file dropped.
fn annotation(text: &str) -> Option<String> {
    let inner = text.strip_prefix('<')?.strip_suffix('>')?;
    Some(inner.strip_suffix(" (deleted)").unwrap_or(inner).to_string())
}

/// A strace string's text after its opening quote, decoded: the C escapes strace prints, octal and
/// hex. A truncated string's `...` lies outside the quotes and is dropped with them.
fn unquote(text: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = text.as_bytes().iter().copied().peekable();
    while let Some(byte) = chars.next() {
        match byte {
            b'"' => break,
            b'\\' => match chars.next() {
                Some(b'n') => bytes.push(b'\n'),
                Some(b't') => bytes.push(b'\t'),
                Some(b'r') => bytes.push(b'\r'),
                Some(b'v') => bytes.push(0x0b),
                Some(b'f') => bytes.push(0x0c),
                Some(b'x') => {
                    let mut value = 0u8;
                    for _ in 0..2 {
                        if let Some(digit) = chars.peek().and_then(|c| (*c as char).to_digit(16)) {
                            value = value * 16 + digit as u8;
                            chars.next();
                        }
                    }
                    bytes.push(value);
                }
                Some(first @ b'0'..=b'7') => {
                    let mut value = (first - b'0') as u32;
                    for _ in 0..2 {
                        if let Some(digit) = chars.peek().filter(|c| (b'0'..=b'7').contains(c)) {
                            value = value * 8 + (digit - b'0') as u32;
                            chars.next();
                        }
                    }
                    bytes.push(value as u8);
                }
                Some(other) => bytes.push(other),
                None => break,
            },
            other => bytes.push(other),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The class of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Mutating,
    Syncing,
    Inert,
}

/// Where a call operates: a path it names, relative to a directory, or a descriptor it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    /// A path argument as passed, with the directory it is relative to: a dirfd's `-y` path, the
    /// `-y` working directory of AT_FDCWD, or None for a legacy call relative to the process's
    /// working directory.
    Named { traced: String, base: Option<String> },
    /// A descriptor, by the path `-y` printed for it.
    Held { target: String },
}

/// A call read against the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Touch {
    pub class: Class,
    /// What it operates on; empty for an inert call.
    pub operands: Vec<Operand>,
    /// What else names the operation: an open's flags, an unlink's or a rename's flags.
    pub detail: String,
}

/// Why a trace cannot be graded.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Unsound {
    /// A call in none of the three classes.
    Unclassified(String),
    /// A write that reaches a file with no call strace sees.
    Unseen(String),
}

impl std::fmt::Display for Unsound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsound::Unclassified(what) => write!(f, "unclassified syscall {}: add it to a class in trace.rs", what),
            Unsound::Unseen(what) => write!(f, "{} writes without a call strace sees", what),
        }
    }
}

/// A path argument: the index of its directory descriptor, if it has one, and its own index.
type PathArgument = (Option<usize>, usize);

/// Calls that name a path, by which arguments are a (directory, path) pair, and whether they
/// mutate it. A directory index of None means the path is relative to the working directory.
const PATH_CALLS: &[(&str, &[PathArgument], Class)] = &[
    ("mkdirat", &[(Some(0), 1)], Class::Mutating),
    ("mkdir", &[(None, 0)], Class::Mutating),
    ("mknodat", &[(Some(0), 1)], Class::Mutating),
    ("mknod", &[(None, 0)], Class::Mutating),
    ("renameat", &[(Some(0), 1), (Some(2), 3)], Class::Mutating),
    ("renameat2", &[(Some(0), 1), (Some(2), 3)], Class::Mutating),
    ("rename", &[(None, 0), (None, 1)], Class::Mutating),
    ("unlinkat", &[(Some(0), 1)], Class::Mutating),
    ("unlink", &[(None, 0)], Class::Mutating),
    ("rmdir", &[(None, 0)], Class::Mutating),
    ("linkat", &[(Some(0), 1), (Some(2), 3)], Class::Mutating),
    ("link", &[(None, 0), (None, 1)], Class::Mutating),
    // A symlink's first argument is its content, not a path it touches.
    ("symlinkat", &[(Some(1), 2)], Class::Mutating),
    ("symlink", &[(None, 1)], Class::Mutating),
    ("fchmodat", &[(Some(0), 1)], Class::Mutating),
    ("fchmodat2", &[(Some(0), 1)], Class::Mutating),
    ("chmod", &[(None, 0)], Class::Mutating),
    ("fchownat", &[(Some(0), 1)], Class::Mutating),
    ("chown", &[(None, 0)], Class::Mutating),
    ("lchown", &[(None, 0)], Class::Mutating),
    ("truncate", &[(None, 0)], Class::Mutating),
    ("setxattr", &[(None, 0)], Class::Mutating),
    ("lsetxattr", &[(None, 0)], Class::Mutating),
    ("removexattr", &[(None, 0)], Class::Mutating),
    ("lremovexattr", &[(None, 0)], Class::Mutating),
    ("utimes", &[(None, 0)], Class::Mutating),
    ("utime", &[(None, 0)], Class::Mutating),
    ("futimesat", &[(Some(0), 1)], Class::Mutating),
];

/// Calls that write or sync through a descriptor, by the index of the descriptor.
const DESCRIPTOR_CALLS: &[(&str, usize, Class)] = &[
    ("write", 0, Class::Mutating),
    ("writev", 0, Class::Mutating),
    ("pwrite64", 0, Class::Mutating),
    ("pwritev", 0, Class::Mutating),
    ("pwritev2", 0, Class::Mutating),
    ("ftruncate", 0, Class::Mutating),
    ("fallocate", 0, Class::Mutating),
    ("fchmod", 0, Class::Mutating),
    ("fchown", 0, Class::Mutating),
    ("fsetxattr", 0, Class::Mutating),
    ("fremovexattr", 0, Class::Mutating),
    ("copy_file_range", 2, Class::Mutating),
    ("sendfile", 0, Class::Mutating),
    ("splice", 2, Class::Mutating),
    ("fsync", 0, Class::Syncing),
    ("fdatasync", 0, Class::Syncing),
    ("syncfs", 0, Class::Syncing),
    ("sync_file_range", 0, Class::Syncing),
];

/// Calls known not to change any file. Opens, mmap and ioctl are judged by their arguments.
const INERT: &[&str] = &[
    "read", "readv", "pread64", "preadv", "preadv2", "close", "close_range", "fstat", "newfstatat", "fstatat64", "stat",
    "lstat", "statx", "lseek", "_llseek", "munmap", "mremap", "mprotect", "madvise", "mlock", "munlock", "brk", "futex",
    "futex_waitv", "getrandom", "clock_gettime", "clock_getres", "clock_nanosleep", "nanosleep", "gettimeofday", "time",
    "rt_sigaction", "rt_sigprocmask", "rt_sigreturn", "rt_sigtimedwait", "rt_sigsuspend", "rt_sigqueueinfo", "sigaltstack",
    "restart_syscall", "sched_yield", "sched_getaffinity", "sched_setaffinity", "sched_getparam", "sched_getscheduler",
    "sched_getattr", "getpid", "gettid", "getppid", "getuid", "geteuid", "getgid", "getegid", "getgroups", "getresuid",
    "getresgid", "getpgrp", "getpgid", "getsid", "setsid", "setpgid", "getcwd", "chdir", "fchdir", "prctl", "arch_prctl",
    "set_tid_address", "set_robust_list", "get_robust_list", "rseq", "clone", "clone3", "fork", "vfork", "execve",
    "execveat", "exit", "exit_group", "wait4", "waitid", "kill", "tgkill", "tkill", "epoll_create", "epoll_create1",
    "epoll_ctl", "epoll_wait", "epoll_pwait", "epoll_pwait2", "eventfd", "eventfd2", "pipe", "pipe2", "socketpair",
    "socket", "connect", "bind", "listen", "accept", "accept4", "sendto", "recvfrom", "sendmsg", "recvmsg", "sendmmsg",
    "recvmmsg", "getsockopt", "setsockopt", "getsockname", "getpeername", "shutdown", "poll", "ppoll", "select",
    "pselect6", "flock", "fcntl", "faccessat", "faccessat2", "access", "readlinkat", "readlink", "getdents64",
    "getdents", "uname", "sysinfo", "prlimit64", "getrlimit", "setrlimit", "dup", "dup2", "dup3", "umask", "membarrier",
    "statfs", "fstatfs", "getcpu", "fadvise64", "timerfd_create", "timerfd_settime", "timerfd_gettime", "signalfd4",
    "getpriority", "setpriority", "capget", "personality", "ioprio_get", "pidfd_open", "pidfd_send_signal", "getxattr",
    "lgetxattr", "fgetxattr", "listxattr", "llistxattr", "flistxattr", "inotify_init1", "inotify_add_watch",
    "inotify_rm_watch", "memfd_create", "getrusage", "times",
];

/// ioctl requests known not to change a file.
const INERT_IOCTLS: &[&str] = &["TCGETS", "TCGETS2", "TIOCGWINSZ", "TIOCGPGRP", "FIONBIO", "FIOCLEX", "FIONCLEX", "FIONREAD"];

/// ioctl requests that write a file: reflink clone and dedupe.
const MUTATING_IOCTLS: &[&str] = &["FICLONE", "FICLONERANGE", "FIDEDUPERANGE", "BTRFS_IOC_CLONE", "BTRFS_IOC_CLONE_RANGE"];

/// Reads `call` against the table.
pub fn touch(call: &Call) -> Result<Touch, Unsound> {
    let inert = Ok(Touch { class: Class::Inert, operands: vec![], detail: String::new() });
    let at = |index: usize| call.arguments.get(index).map(|text| argument(text));
    let named = |directory: Option<usize>, path: usize| -> Option<Operand> {
        let Some(Argument::Text(traced)) = at(path) else { return None };
        let base = match directory.and_then(at) {
            Some(Argument::Descriptor { target, .. }) => Some(target),
            Some(Argument::Cwd(cwd)) => cwd,
            _ => None,
        };
        Some(Operand::Named { traced, base })
    };
    let held = |index: usize| -> Option<Operand> {
        match at(index) {
            Some(Argument::Descriptor { target, .. }) => Some(Operand::Held { target }),
            _ => None,
        }
    };
    let name = call.name.as_str();

    if let Some((_, pairs, class)) = PATH_CALLS.iter().find(|(known, _, _)| *known == name) {
        let operands: Vec<Operand> = pairs.iter().filter_map(|(directory, path)| named(*directory, *path)).collect();
        let detail = match name {
            "unlinkat" => call.arguments.get(2).cloned().unwrap_or_default(),
            "renameat2" => call.arguments.get(4).cloned().unwrap_or_default(),
            _ => String::new(),
        };
        return Ok(Touch { class: *class, operands, detail });
    }
    if let Some((_, index, class)) = DESCRIPTOR_CALLS.iter().find(|(known, _, _)| *known == name) {
        return Ok(Touch { class: *class, operands: held(*index).into_iter().collect(), detail: String::new() });
    }
    match name {
        "openat" | "open" | "creat" | "openat2" => {
            let (operand, flags) = match name {
                "openat" => (named(Some(0), 1), call.arguments.get(2).cloned().unwrap_or_default()),
                "open" => (named(None, 0), call.arguments.get(1).cloned().unwrap_or_default()),
                "creat" => (named(None, 0), "O_CREAT|O_WRONLY|O_TRUNC".to_string()),
                _ => (named(Some(0), 1), call.arguments.get(2).cloned().unwrap_or_default()),
            };
            let writes = ["O_CREAT", "O_TRUNC", "O_WRONLY", "O_RDWR"].iter().any(|flag| flags.contains(flag));
            if !writes {
                return inert;
            }
            Ok(Touch { class: Class::Mutating, operands: operand.into_iter().collect(), detail: flags })
        }
        "utimensat" => {
            // A null path sets the times of the dirfd itself.
            let operand = named(Some(0), 1).or_else(|| held(0));
            Ok(Touch { class: Class::Mutating, operands: operand.into_iter().collect(), detail: String::new() })
        }
        "sync" => Ok(Touch { class: Class::Syncing, operands: vec![], detail: String::new() }),
        "mmap" | "mmap2" => {
            let protection = call.arguments.get(2).map(String::as_str).unwrap_or_default();
            let flags = call.arguments.get(3).map(String::as_str).unwrap_or_default();
            let file = matches!(at(4), Some(Argument::Descriptor { .. }));
            if file && protection.contains("PROT_WRITE") && flags.contains("MAP_SHARED") {
                return Err(Unsound::Unseen(format!("a shared writable mapping: mmap({})", call.arguments.join(", "))));
            }
            inert
        }
        "ioctl" => {
            let request = call.arguments.get(1).map(String::as_str).unwrap_or_default();
            if MUTATING_IOCTLS.contains(&request) {
                return Ok(Touch { class: Class::Mutating, operands: held(0).into_iter().collect(), detail: request.to_string() });
            }
            if INERT_IOCTLS.contains(&request) {
                return inert;
            }
            Err(Unsound::Unclassified(format!("ioctl {}", request)))
        }
        name if name.starts_with("io_uring") => Err(Unsound::Unseen(format!("io_uring ({})", name))),
        name if INERT.contains(&name) => inert,
        name => Err(Unsound::Unclassified(name.to_string())),
    }
}

/// The thread group of every thread in a trace: a clone with CLONE_THREAD joins its parent's, any
/// other starts its own. The first thread in the trace is the traced command's own process.
pub fn thread_groups(calls: &[Call]) -> BTreeMap<u32, u32> {
    let mut groups = BTreeMap::new();
    let Some(first) = calls.first() else { return groups };
    groups.insert(first.tid, first.tid);
    let mut spawned: Vec<(u32, u32, bool)> = Vec::new();
    for call in calls.iter().filter(|call| matches!(call.name.as_str(), "clone" | "clone3" | "fork" | "vfork")) {
        let Returned::Value(child) = &call.returned else { continue };
        let Ok(child) = child.parse::<u32>() else { continue };
        if child == 0 {
            continue;
        }
        spawned.push((call.tid, child, call.arguments.iter().any(|argument| argument.contains("CLONE_THREAD"))));
    }
    // A child's clone can print before its parent's: settle until nothing changes. A chain settles
    // within one pass per spawn; a tid reused under two parents never does, so the passes stop
    // there rather than hang.
    for _ in 0..=spawned.len() {
        let mut changed = false;
        for (parent, child, thread) in &spawned {
            let Some(&group) = groups.get(parent) else { continue };
            let wanted = if *thread { group } else { *child };
            if groups.insert(*child, wanted) != Some(wanted) {
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    for call in calls {
        groups.entry(call.tid).or_insert(call.tid);
    }
    groups
}

/// A path made absolute against `base`, with `.` and `..` components resolved lexically.
pub fn resolve(path: &str, base: &str) -> String {
    let joined = if path.starts_with('/') { path.to_string() } else { format!("{}/{}", base, path) };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// A scratch root in both spellings: as the harness passed it, and symlink-free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub given: String,
    pub canonical: String,
}

impl Root {
    /// The root at `path`, which must exist.
    pub fn of(path: &std::path::Path) -> std::io::Result<Root> {
        let given = path.to_string_lossy().trim_end_matches('/').to_string();
        let canonical = std::fs::canonicalize(path)?.to_string_lossy().trim_end_matches('/').to_string();
        Ok(Root { given, canonical })
    }

    /// The spellings, longest first, so a canonical root that extends the given one wins.
    fn spellings(&self) -> [&str; 2] {
        if self.canonical.len() >= self.given.len() { [&self.canonical, &self.given] } else { [&self.given, &self.canonical] }
    }

    /// The rest of `path` below this root, at a component boundary, with which spelling matched
    /// (0 the given one, 1 the canonical); None when the path is not under it.
    fn below<'p>(&self, path: &'p str) -> Option<(&'p str, bool)> {
        for spelling in self.spellings() {
            if let Some(rest) = path.strip_prefix(spelling) {
                if rest.is_empty() || rest.starts_with('/') {
                    return Some((rest, spelling == self.canonical));
                }
            }
        }
        None
    }

    pub fn holds(&self, path: &str) -> bool {
        self.below(path).is_some()
    }

    /// `path` moved from under this root to under `onto`, keeping the spelling it had.
    pub fn rebase(&self, path: &str, onto: &Root) -> Option<String> {
        let (rest, canonical) = self.below(path)?;
        Some(format!("{}{}", if canonical { &onto.canonical } else { &onto.given }, rest))
    }

    /// `path` with this root, in either spelling, replaced by `<root>`.
    pub fn tokenized(&self, path: &str) -> String {
        match self.below(path) {
            Some((rest, _)) => format!("<root>{}", rest),
            None => path.to_string(),
        }
    }
}

/// `path` with every `<pid>-<n>` of the process `pid` replaced by `<pid>-<n>`: a temp name's
/// process id and counter, which differ from run to run. True when anything was replaced.
pub fn without_temp(path: &str, pid: u32) -> (String, bool) {
    let needle = format!("{}-", pid);
    let mut out = String::new();
    let mut rest = path;
    let mut changed = false;
    while let Some(at) = rest.find(&needle) {
        let before_ok = at == 0 || !rest.as_bytes()[at - 1].is_ascii_digit();
        let after = &rest[at + needle.len()..];
        let digits = after.find(|c: char| !c.is_ascii_digit()).unwrap_or(after.len());
        if before_ok && digits > 0 {
            out.push_str(&rest[..at]);
            out.push_str("<pid>-<n>");
            rest = &after[digits..];
            changed = true;
        } else {
            out.push_str(&rest[..at + needle.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    (out, changed)
}

/// A path's base for grouping kill points into units: every hash-like run (a two-character shard
/// component, or sixteen or more hex digits) replaced by `<hex>`, so every object is one base and a
/// head publish is another.
pub fn base(path: &str) -> String {
    path.split('/').map(|component| {
        let stem = component.split('.').next().unwrap_or_default();
        if stem.len() == 2 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("<hex>{}", &component[2..]);
        }
        let mut out = String::new();
        let mut run = String::new();
        for c in component.chars().chain(std::iter::once('/')) {
            if c.is_ascii_hexdigit() && c != '/' {
                run.push(c);
                continue;
            }
            if run.len() >= 16 { out.push_str("<hex>") } else { out.push_str(&run) }
            run.clear();
            if c != '/' {
                out.push(c);
            }
        }
        out
    }).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trace's lines become calls: an unfinished call joins its resumed half, a killed call
    /// returns unknown, a failure keeps its errno, an interrupted one is restarted, and exit and
    /// signal lines are no calls. A line that is not a call refuses the trace.
    ///
    /// Revert: drop the unfinished/resumed join → the fsync's descriptor is lost and the resumed
    /// line refuses the trace.
    #[test]
    fn a_trace_reads_as_calls() {
        let text = "87    execve(\"/b/forklift\", [\"forklift\", \"--json\"], 0xffff /* 3 vars */) = 0\n\
            88    fsync(12</s/work/.forklift/pallets> <unfinished ...>\n\
            87    unlinkat(AT_FDCWD</s/work>, \".forklift/lock\", 0) = -1 ENOENT (No such file or directory)\n\
            88    <... fsync resumed>) = ?\n\
            87    futex(0x1, FUTEX_WAIT, 0, NULL) = ? ERESTARTSYS (To be restarted if SA_RESTART is set)\n\
            87    --- SIGCHLD {si_signo=SIGCHLD} ---\n\
            88    +++ killed by SIGKILL +++\n";
        let calls = parse(text).unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0].arguments.len(), 3);
        assert_eq!(calls[1].name, "unlinkat");
        assert_eq!(calls[1].returned, Returned::Failed("ENOENT".into()));
        assert_eq!(calls[2], Call { tid: 88, name: "fsync".into(), arguments: vec!["12</s/work/.forklift/pallets>".into()], returned: Returned::Unknown });
        assert_eq!(calls[3].returned, Returned::Restarted);
        assert!(parse("87    nonsense\n").is_err());
        assert!(parse("87    <... fsync resumed>) = 0\n").is_err(), "a resumed call that never started");
        // A call cut off by the kill with no resumed line never returned.
        let cut = parse("88    write(5</s/a.tmp~87-0>, \"x\", 1 <unfinished ...>\n").unwrap();
        assert_eq!(cut[0].returned, Returned::Unknown);
    }

    /// Arguments split at top-level commas only, and each reads as what it names.
    ///
    /// Revert: split inside quotes → a path holding a comma becomes two arguments; drop the
    /// annotation rule → `3</a,b>` splits.
    #[test]
    fn arguments_split_and_read_as_what_they_name() {
        let (arguments, after) = split_arguments("AT_FDCWD</s/w>, \"a,\\\"b\", O_RDONLY|O_CLOEXEC, {st_mode=S_IFREG, st_size=3}, 3</x,y>) = 3</x>").unwrap();
        assert_eq!(arguments.len(), 5);
        assert_eq!(after, " = 3</x>");
        assert_eq!(argument(&arguments[0]), Argument::Cwd(Some("/s/w".into())));
        assert_eq!(argument(&arguments[1]), Argument::Text("a,\"b".into()));
        assert_eq!(argument(&arguments[2]), Argument::Other("O_RDONLY|O_CLOEXEC".into()));
        assert_eq!(argument(&arguments[4]), Argument::Descriptor { fd: 3, target: "/x,y".into() });
        assert_eq!(argument("4<anon_inode:[eventfd]>"), Argument::Descriptor { fd: 4, target: "anon_inode:[eventfd]".into() });
        assert_eq!(argument("5</w/gone (deleted)>"), Argument::Descriptor { fd: 5, target: "/w/gone".into() });
        assert_eq!(argument("\"\\303\\237\\x41\"..."), Argument::Text("ßA".into()));
        assert_eq!(split_arguments(") = 0").unwrap().0, Vec::<String>::new());
    }

    fn one(line: &str) -> Call {
        parse(&format!("7 {}\n", line)).unwrap().remove(0)
    }

    /// The table classifies by name and arguments: a writing open mutates and a read-only one does
    /// not, a rename touches both its paths, a descriptor write names its file, a shared writable
    /// file mapping and io_uring are unsound, and an unknown call or ioctl is unclassified.
    ///
    /// Revert: classify by name alone → a read-only open is a kill point; drop the mmap rule → a
    /// shared writable mapping grades as sound.
    #[test]
    fn calls_classify_against_three_closed_classes() {
        let open = touch(&one("openat(AT_FDCWD</s/w>, \".forklift/lock\", O_WRONLY|O_CREAT|O_EXCL, 0644) = 3</s/w/.forklift/lock>")).unwrap();
        assert_eq!(open.class, Class::Mutating);
        assert_eq!(open.operands, vec![Operand::Named { traced: ".forklift/lock".into(), base: Some("/s/w".into()) }]);
        assert_eq!(touch(&one("openat(AT_FDCWD</s/w>, \"a\", O_RDONLY|O_CLOEXEC) = 3</s/w/a>")).unwrap().class, Class::Inert);
        let rename = touch(&one("rename(\".forklift/pallets/main.tmp~87-3\", \".forklift/pallets/main\") = 0")).unwrap();
        assert_eq!(rename.operands.len(), 2);
        assert_eq!(rename.operands[1], Operand::Named { traced: ".forklift/pallets/main".into(), base: None });
        let write = touch(&one("write(11</s/w/.forklift/x>, \"abc\", 3) = 3")).unwrap();
        assert_eq!((write.class, write.operands), (Class::Mutating, vec![Operand::Held { target: "/s/w/.forklift/x".into() }]));
        assert_eq!(touch(&one("fsync(12</s/w/.forklift>) = 0")).unwrap().class, Class::Syncing);
        assert_eq!(touch(&one("ioctl(1</dev/null>, TIOCGWINSZ, 0xffff) = -1 ENOTTY (x)")).unwrap().class, Class::Inert);
        assert!(matches!(touch(&one("ioctl(3</s/a>, FS_IOC_GETFLAGS, 0x1) = 0")), Err(Unsound::Unclassified(_))));
        assert_eq!(touch(&one("ioctl(4</s/b>, FICLONE, 3) = 0")).unwrap().class, Class::Mutating);
        assert!(matches!(touch(&one("mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_SHARED, 3</s/a>, 0) = 0xffff")), Err(Unsound::Unseen(_))));
        assert_eq!(touch(&one("mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0) = 0xffff")).unwrap().class, Class::Inert);
        assert_eq!(touch(&one("mmap(NULL, 4096, PROT_READ, MAP_SHARED, 3</s/a>, 0) = 0xffff")).unwrap().class, Class::Inert);
        assert!(matches!(touch(&one("io_uring_setup(8, 0xffff) = 3")), Err(Unsound::Unseen(_))));
        assert_eq!(touch(&one("frobnicate(1) = 0")), Err(Unsound::Unclassified("frobnicate".into())));
    }

    /// A thread clone joins its parent's group and a process clone starts its own, whichever line
    /// prints first.
    ///
    /// Revert: take every clone as a thread → a child git's calls count as the tool's own; settle
    /// with no bound → a reused tid hangs the test.
    #[test]
    fn threads_join_their_parents_group() {
        let calls = parse("10 execve(\"/f\", [], 0x0) = 0\n\
            12 write(1<pipe:[5]>, \"x\", 1) = 1\n\
            10 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD|CLONE_SIGHAND) = 11\n\
            11 clone(child_stack=NULL, flags=CLONE_CHILD_SETTID|SIGCHLD) = 12\n\
            12 clone3({flags=CLONE_VM|CLONE_THREAD, exit_signal=0}, 88) = 13\n").unwrap();
        let groups = thread_groups(&calls);
        assert_eq!(groups[&10], 10);
        assert_eq!(groups[&11], 10);
        assert_eq!(groups[&12], 12);
        assert_eq!(groups[&13], 12);
        // A tid reused under two parents wanting different groups settles nowhere; it must not hang.
        let reused = parse("10 execve(\"/f\", [], 0x0) = 0\n\
            10 clone(child_stack=NULL, flags=SIGCHLD) = 60\n\
            10 clone(child_stack=0x1, flags=CLONE_VM|CLONE_THREAD) = 60\n").unwrap();
        assert!(thread_groups(&reused).contains_key(&60));
    }

    /// A root covers a path only at a component boundary, in either spelling, and rebases keeping
    /// the spelling the path had.
    ///
    /// Revert: match by plain prefix → /s covers /srv; rebase every spelling onto the given one →
    /// a `-y` path keeps a root the injected run does not resolve to.
    #[test]
    fn roots_cover_paths_at_component_boundaries_in_both_spellings() {
        let counting = Root { given: "/tmp/o/0001-c".into(), canonical: "/private/tmp/o/0001-c".into() };
        let injected = Root { given: "/tmp/o/0001-c-kill-3".into(), canonical: "/private/tmp/o/0001-c-kill-3".into() };
        assert!(counting.holds("/tmp/o/0001-c/work/a"));
        assert!(counting.holds("/private/tmp/o/0001-c"));
        assert!(!counting.holds("/tmp/o/0001-c-kill-3/work"));
        assert_eq!(counting.rebase("/private/tmp/o/0001-c/home", &injected).as_deref(), Some("/private/tmp/o/0001-c-kill-3/home"));
        assert_eq!(counting.rebase("/tmp/o/0001-c/work/x", &injected).as_deref(), Some("/tmp/o/0001-c-kill-3/work/x"));
        assert_eq!(counting.rebase("/srv/x", &injected), None);
        assert_eq!(counting.tokenized("/private/tmp/o/0001-c/work/x"), "<root>/work/x");
        assert_eq!(resolve("../b/./c", "/s/w/a"), "/s/w/b/c");
        assert_eq!(resolve("/x//y", "/s"), "/x/y");
    }

    /// A temp name loses its pid and counter wherever they sit in it, and nothing else does.
    ///
    /// Revert: replace any digits after a dash → a shard or a record number is taken for a temp.
    #[test]
    fn temp_names_lose_their_pid_and_counter() {
        assert_eq!(without_temp(".forklift/pallets/main.tmp~87-3", 87), (".forklift/pallets/main.tmp~<pid>-<n>".into(), true));
        assert_eq!(without_temp("/t/forklift-export-87-0-ab.tmp", 87), ("/t/forklift-export-<pid>-<n>-ab.tmp".into(), true));
        assert_eq!(without_temp(".compact-87-12.pack.tmp", 87), (".compact-<pid>-<n>.pack.tmp".into(), true));
        assert_eq!(without_temp(".forklift/graph/187-3", 87), (".forklift/graph/187-3".into(), false));
        assert_eq!(without_temp(".forklift/pallets/main", 87), (".forklift/pallets/main".into(), false));
        assert_eq!(base(".forklift/objects/92/0f3a5e9bc1d2e3f4a5b6c7d8e9f00112.tmp~<pid>-<n>"), ".forklift/objects/<hex>/<hex>.tmp~<pid>-<n>");
        assert_eq!(base(".forklift/pallets/main"), ".forklift/pallets/main");
        assert_eq!(base(".forklift/inventory/inv_0123456789abcdef0123/data"), ".forklift/inventory/inv_<hex>/data");
    }
}
