//! The progress-events channel: Pult's events protocol v1, written to the file descriptor named
//! by `PULT_EVENTS` (see `docs/MACHINE_INTERFACE.md`, "Progress events").
//!
//! Forklift never owns that channel — whoever set the variable does — so it writes only
//! `progress` and `status` lines and never `step`. The core operations report into a
//! [`Progress`] handle; [`progress`] hands out one wired to this channel, or an inactive one when
//! there is no usable channel.
//!
//! Emission must never block, slow or kill the operation it reports on. So:
//!
//! * The fd is validated once, at process start, before Forklift opens a file of its own — an
//!   inherited number that is closed, or now names a file, is never written to. Whatever fd the
//!   variable names is not passed on to children even when it is refused, since a refused write
//!   end that a child kept would hold the reader open.
//! * Reports go into a small bounded queue and a dedicated thread writes them out. A push never
//!   waits on the reader: it coalesces into the newest entry where that loses nothing a reader
//!   needs, and otherwise drops the oldest entry when the queue is full.
//! * A reader that has gone away must not kill the process. Forklift runs with `SIGPIPE` at its
//!   default disposition (see `pager::restore_sigpipe`), so a write to a closed pipe would
//!   terminate it. No single mechanism prevents that everywhere: Darwin delivers that `SIGPIPE`
//!   process-wide, so a thread mask does not help and the fd needs `F_SETNOSIGPIPE` (a pipe or
//!   FIFO) or `SO_NOSIGPIPE` (a socket, which ignores the fd flag); Linux delivers it to the
//!   writing thread and has no such fd flag, so the writer thread blocks the signal. Other
//!   platforms have neither mechanism verified, and never open the channel.
//! * The channel belongs to this process alone. `PULT_EVENTS` is removed from the environment
//!   and the fd it names is made close-on-exec whether or not it is accepted, so a child Forklift
//!   spawns (a `forklift` per MCP tool call, `git`, a shell) neither writes into the channel nor
//!   holds it open.

// Off macOS and Linux nothing opens a channel, so the line and queue machinery is unused there.
#![cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]

use forklift_core::util::progress_utils::{Event, Progress};

/// The environment variable naming the channel's file descriptor.
const VARIABLE: &str = "PULT_EVENTS";

/// The channel opened at startup, if there is a usable one. Unset until [`init`] runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
static CHANNEL: std::sync::OnceLock<Option<std::sync::Arc<channel::Channel>>> = std::sync::OnceLock::new();

/// Validate and open the channel `PULT_EVENTS` names. Call once, first thing in `main`: the
/// validation is only sound before this process has opened any file descriptor of its own,
/// since a stale inherited number could otherwise have been reused for one of them.
pub fn init() {
    let value = std::env::var_os(VARIABLE);

    // A child must not inherit the variable: it would name a channel this process owns now, or
    // one this process refused. Sound only because no other thread exists yet to read the
    // environment concurrently.
    if value.is_some() {
        std::env::remove_var(VARIABLE);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let opened = value.and_then(|value| channel::open(&value));
        let _ = CHANNEL.set(opened);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = value;
}

/// A reporting handle for one operation: wired to the channel when there is one, inactive
/// otherwise.
pub fn progress() -> Progress {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(Some(channel)) = CHANNEL.get() {
        let channel = std::sync::Arc::clone(channel);
        return Progress::new(move |event| channel.push(Entry::from_event(event)));
    }

    Progress::NONE
}

/// Give the writer a bounded moment to send what is still queued, so a short command's last
/// reports reach the reader instead of dying with the process. Returns at once when there is
/// no channel or nothing is queued, and within [`DRAIN_BUDGET`] when the reader is not reading.
pub fn finish() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(Some(channel)) = CHANNEL.get() {
        channel.drain(DRAIN_BUDGET);
    }
}

/// How many reports may wait for the writer. Coalescing keeps a steady stream of progress to one
/// entry, so this only fills when phases change faster than the reader reads.
const QUEUE_CAPACITY: usize = 64;

/// The longest line written, newline included. POSIX guarantees a pipe write of at most
/// `PIPE_BUF` bytes is atomic, and 512 is the smallest `PIPE_BUF` it allows — so a line never
/// interleaves with another process's line on a shared channel (a parent's own events, say).
const MAX_LINE_BYTES: usize = 512;

/// The longest [`finish`] waits for the writer before the process exits anyway.
const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// One queued line, kept with what coalescing needs to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    /// A `progress` line, with its percentage (`None` for `?`).
    Progress { percent: Option<u8>, line: String },

    /// A `status` line.
    Status { line: String },
}

impl Entry {
    fn from_event(event: Event<'_>) -> Entry {
        match event {
            Event::Status(phase) => Entry::Status { line: format_line("status ", phase) },
            Event::Progress { percent, text } => {
                let head = match percent {
                    Some(value) => format!("progress {} ", value.min(100)),
                    None => "progress ? ".to_string(),
                };
                Entry::Progress { percent, line: format_line(&head, text) }
            }
        }
    }

    fn line(&self) -> &str {
        match self {
            Entry::Progress { line, .. } | Entry::Status { line } => line,
        }
    }
}

/// Whether `c` must not reach a line: a control character, a character some readers treat as
/// a line break (U+2028, U+2029), or a bidirectional formatting character, which can make the
/// displayed text (a file path, say) read differently from what it is.
fn is_unsafe_in_a_line(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{061C}' | '\u{200E}' | '\u{200F}'
                       | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Build one event line: `head` (the verb and its fixed arguments, ending in a space), then the
/// text with every [unsafe character](is_unsafe_in_a_line) turned into a space, trimmed, cut to fit
/// [`MAX_LINE_BYTES`] on a character boundary, and a newline.
fn format_line(head: &str, text: &str) -> String {
    let clean: String = text.chars().map(|c| if is_unsafe_in_a_line(c) { ' ' } else { c }).collect();
    let clean = clean.trim();

    let mut line = String::with_capacity(head.len() + clean.len() + 1);
    line.push_str(head);

    let room = MAX_LINE_BYTES - 1 - head.len();
    let mut end = clean.len().min(room);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    line.push_str(&clean[..end]);

    // `status` with no text is malformed, and `progress 40 ` reads the same as `progress 40`.
    let trimmed = line.trim_end().len();
    line.truncate(trimmed);
    line.push('\n');
    line
}

/// Add `entry` to the queue under the protocol's coalescing rules:
///
/// * a `progress` replaces a preceding `progress` of no greater value — the newer one says
///   everything the older one did. An indeterminate (`?`) one has no value to compare, so it
///   replaces only another indeterminate one;
/// * a `status` replaces a preceding `status`;
/// * otherwise the entry is appended, and when the queue is full the oldest entry is dropped.
///
/// Only the newest queued entry is ever replaced, so the order of what is written is the order
/// of what was reported. A reset (a lower value after a `status`) can still be lost, to a later
/// higher value or to overflow — readers treat resets as best-effort.
fn enqueue(queue: &mut std::collections::VecDeque<Entry>, entry: Entry, capacity: usize) {
    let replaces = match (queue.back(), &entry) {
        (Some(Entry::Progress { percent: older, .. }), Entry::Progress { percent: newer, .. }) => {
            match (older, newer) {
                (Some(older), Some(newer)) => older <= newer,
                (None, None) => true,
                _ => false,
            }
        }
        (Some(Entry::Status { .. }), Entry::Status { .. }) => true,
        _ => false,
    };

    if replaces {
        if let Some(back) = queue.back_mut() {
            *back = entry;
        }
        return;
    }

    if queue.len() >= capacity {
        queue.pop_front();
    }
    queue.push_back(entry);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod channel {
    use std::collections::VecDeque;
    use std::ffi::OsStr;
    use std::os::unix::io::RawFd;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};
    use super::{enqueue, Entry, QUEUE_CAPACITY};

    /// `fcntl` command: never raise `SIGPIPE` for a write to this fd, return `EPIPE` instead.
    /// The value is Darwin's `sys/fcntl.h`; the `libc` crate (0.2.186) defines it only for
    /// NetBSD, so it is spelled out here.
    #[cfg(target_os = "macos")]
    pub(super) const F_SETNOSIGPIPE: libc::c_int = 73;

    /// The open channel: the queue, and the writer thread draining it.
    pub(super) struct Channel {
        state: Mutex<State>,

        /// Signalled when an entry is queued.
        queued: Condvar,

        /// Signalled when the writer finishes a line or gives up.
        written: Condvar,
    }

    struct State {
        queue: VecDeque<Entry>,

        /// Whether the writer holds a line it has taken off the queue but not finished writing.
        writing: bool,

        /// Whether the channel is unusable (the reader went away, or a write failed): pushes are
        /// discarded from then on.
        dead: bool,
    }

    /// Open the channel `value` names, or `None` when it does not name a usable one.
    pub(super) fn open(value: &OsStr) -> Option<Arc<Channel>> {
        let fd = parse_fd(value)?;

        // Close-on-exec before any check can refuse the fd: a refused write end held open by a
        // child, or by a daemon it leaves behind, would keep the reader from ever seeing EOF.
        // SAFETY: `fcntl` with an integer argument has no memory effects; on a number that is not
        // open it fails with `EBADF` and changes nothing. The flag belongs to this process's
        // descriptor, not the shared open file description, so it changes nothing for anyone else
        // holding the fd.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return None;
        }

        let stream = writable_stream(fd)?;

        #[cfg(target_os = "macos")]
        if !set_no_sigpipe(fd, stream) {
            return None;
        }

        #[cfg(not(target_os = "macos"))]
        let _ = stream;

        Some(start(fd))
    }

    /// Make a write to a reader that has gone return `EPIPE` instead of raising `SIGPIPE`. A pipe
    /// or FIFO takes the fd flag; a socket ignores that flag (`fcntl` refuses it) and takes the
    /// socket option instead. Either lives on the shared open file description or socket, so it
    /// holds for every process writing to this channel, and outlasts this one.
    #[cfg(target_os = "macos")]
    fn set_no_sigpipe(fd: RawFd, stream: Stream) -> bool {
        let on: libc::c_int = 1;

        // SAFETY: `fcntl` with an integer argument has no memory effects; `setsockopt` reads
        // `size_of::<c_int>()` bytes from `on`, which outlives the call.
        unsafe {
            match stream {
                Stream::Pipe => libc::fcntl(fd, F_SETNOSIGPIPE, 1) != -1,
                Stream::Socket => libc::setsockopt(
                    fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE,
                    (&on as *const libc::c_int).cast(), std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                ) == 0,
            }
        }
    }

    /// Start the writer thread over a validated fd.
    pub(super) fn start(fd: RawFd) -> Arc<Channel> {
        let channel = Arc::new(Channel {
            state: Mutex::new(State { queue: VecDeque::new(), writing: false, dead: false }),
            queued: Condvar::new(),
            written: Condvar::new(),
        });

        let writer = Arc::clone(&channel);
        let spawned = std::thread::Builder::new()
            .name("forklift-events".to_string())
            .spawn(move || writer.run(fd));

        if spawned.is_err() {
            channel.lock().dead = true;
        }

        channel
    }

    /// Parse the variable's value: bare decimal digits naming an fd above stderr. Anything else
    /// — a sign, whitespace, an overflow — is no channel.
    pub(super) fn parse_fd(value: &OsStr) -> Option<RawFd> {
        let text = value.to_str()?;

        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }

        // Never 0, 1 or 2: stdout carries the `--json` document, and the others are not ours.
        text.parse::<RawFd>().ok().filter(|fd| *fd > 2)
    }

    /// What kind of stream a channel fd is.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stream {
        /// A pipe or FIFO.
        Pipe,

        /// A socket.
        Socket,
    }

    /// The kind of stream `fd` is, if it is open for writing and is a pipe, FIFO or socket —
    /// anything else (a regular file, a terminal, a closed number) is a stale inheritance, not a
    /// channel.
    pub(super) fn writable_stream(fd: RawFd) -> Option<Stream> {
        // SAFETY: `fstat` writes only into the zeroed `stat` we own; `fcntl(F_GETFL)` reads flags.
        let (stat_ok, mode, flags) = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            let stat_ok = libc::fstat(fd, &mut stat) == 0;
            (stat_ok, stat.st_mode, libc::fcntl(fd, libc::F_GETFL))
        };

        if !stat_ok || flags == -1 {
            return None;
        }

        let access = flags & libc::O_ACCMODE;
        if access != libc::O_WRONLY && access != libc::O_RDWR {
            return None;
        }

        match mode & libc::S_IFMT {
            libc::S_IFIFO => Some(Stream::Pipe),
            libc::S_IFSOCK => Some(Stream::Socket),
            _ => None,
        }
    }

    impl Channel {
        fn lock(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Queue an entry. Never waits on the writer or the reader.
        pub(super) fn push(&self, entry: Entry) {
            let mut state = self.lock();

            if state.dead {
                return;
            }

            enqueue(&mut state.queue, entry, QUEUE_CAPACITY);
            drop(state);
            self.queued.notify_one();
        }

        /// Wait up to `budget` for the queue to empty and the line in flight to finish.
        pub(super) fn drain(&self, budget: Duration) {
            let deadline = Instant::now() + budget;
            let mut state = self.lock();

            while !state.dead && (state.writing || !state.queue.is_empty()) {
                let now = Instant::now();
                if now >= deadline {
                    return;
                }

                state = self.written.wait_timeout(state, deadline - now)
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .0;
            }
        }

        /// The writer thread: take lines off the queue in order and write each one whole.
        fn run(&self, fd: RawFd) {
            if !insulate_thread() {
                self.give_up();
                return;
            }

            loop {
                let line = {
                    let mut state = self.lock();

                    while state.queue.is_empty() {
                        state = self.queued.wait(state).unwrap_or_else(|poisoned| poisoned.into_inner());
                    }

                    state.writing = true;
                    state.queue.pop_front().expect("the queue was checked non-empty under the lock")
                };

                let sent = write_all(fd, line.line().as_bytes());

                if !sent {
                    self.give_up();
                    return;
                }

                self.lock().writing = false;
                self.written.notify_all();
            }
        }

        /// Stop for good: discard what is queued and every later push.
        fn give_up(&self) {
            let mut state = self.lock();
            state.dead = true;
            state.writing = false;
            state.queue.clear();
            drop(state);
            self.written.notify_all();
        }
    }

    /// Make this thread's writes survive a reader that has gone away. On Linux, a write's
    /// `SIGPIPE` goes to the writing thread, so blocking it here leaves the write returning
    /// `EPIPE`; the signal stays pending on this thread and is never delivered. Darwin needs
    /// nothing here — its fd flag was set when the channel opened.
    fn insulate_thread() -> bool {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: builds a signal set we own and changes only this thread's mask.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGPIPE);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == 0
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            true
        }
    }

    /// Write all of `bytes`, retrying an interrupted write and waiting out a full non-blocking
    /// fd. `false` when the channel is unusable (`EPIPE` from a reader that has gone, or any
    /// other error).
    fn write_all(fd: RawFd, mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            // SAFETY: writes from a live slice of exactly the length passed.
            let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };

            if written >= 0 {
                bytes = &bytes[written as usize..];
                continue;
            }

            match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::Interrupted => {}
                std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                _ => return false,
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use super::*;

    fn progress(percent: Option<u8>, text: &str) -> Entry {
        Entry::from_event(Event::Progress { percent, text })
    }

    fn status(text: &str) -> Entry {
        Entry::from_event(Event::Status(text))
    }

    fn lines(queue: &VecDeque<Entry>) -> Vec<&str> {
        queue.iter().map(Entry::line).collect()
    }

    #[test]
    fn lines_follow_the_protocol_verbs() {
        assert_eq!(progress(Some(40), "3/7 objects").line(), "progress 40 3/7 objects\n");
        assert_eq!(progress(None, "12 parcels walked").line(), "progress ? 12 parcels walked\n");
        assert_eq!(progress(Some(100), "").line(), "progress 100\n");
        assert_eq!(status("Packing objects").line(), "status Packing objects\n");
    }

    #[test]
    fn control_characters_never_reach_a_line() {
        let line = progress(Some(1), "a\nb\rc\td\u{1b}[31me\u{7f}").line().to_string();
        assert_eq!(line, "progress 1 a b c d [31me\n");
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn line_separators_and_bidi_controls_never_reach_a_line() {
        let line = progress(None, "a\u{2028}b\u{2029}c\u{202E}d\u{2067}e\u{200F}f").line().to_string();
        assert_eq!(line, "progress ? a b c d e f\n");
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_LINE_BYTES);
        let line = progress(Some(5), &text).line().to_string();
        assert!(line.len() <= MAX_LINE_BYTES, "{} bytes", line.len());
        assert!(line.len() >= MAX_LINE_BYTES - 1, "cut too short: {} bytes", line.len());
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn a_higher_progress_replaces_the_newest_progress() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, progress(Some(10), "a"), 8);
        enqueue(&mut queue, progress(Some(10), "b"), 8);
        enqueue(&mut queue, progress(Some(20), "c"), 8);
        assert_eq!(lines(&queue), vec!["progress 20 c\n"]);
    }

    #[test]
    fn a_lower_progress_is_queued_after_it() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, progress(Some(20), "a"), 8);
        enqueue(&mut queue, progress(Some(10), "b"), 8);
        assert_eq!(lines(&queue), vec!["progress 20 a\n", "progress 10 b\n"]);
    }

    #[test]
    fn indeterminate_progress_replaces_only_indeterminate_progress() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, progress(None, "a"), 8);
        enqueue(&mut queue, progress(None, "b"), 8);
        enqueue(&mut queue, progress(Some(0), "c"), 8);
        enqueue(&mut queue, progress(None, "d"), 8);
        assert_eq!(lines(&queue), vec!["progress ? b\n", "progress 0 c\n", "progress ? d\n"]);
    }

    #[test]
    fn a_status_replaces_only_a_status() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, status("one"), 8);
        enqueue(&mut queue, status("two"), 8);
        enqueue(&mut queue, progress(Some(50), "x"), 8);
        enqueue(&mut queue, status("three"), 8);
        assert_eq!(lines(&queue), vec!["status two\n", "progress 50 x\n", "status three\n"]);
    }

    #[test]
    fn progress_never_merges_across_a_status() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, progress(Some(90), "a"), 8);
        enqueue(&mut queue, status("next"), 8);
        enqueue(&mut queue, progress(Some(95), "b"), 8);
        assert_eq!(lines(&queue), vec!["progress 90 a\n", "status next\n", "progress 95 b\n"]);
    }

    #[test]
    fn a_full_queue_drops_its_oldest_entry() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, status("first"), 3);
        enqueue(&mut queue, progress(Some(50), "second"), 3);
        enqueue(&mut queue, status("third"), 3);
        enqueue(&mut queue, progress(Some(10), "fourth"), 3);
        assert_eq!(lines(&queue), vec!["progress 50 second\n", "status third\n", "progress 10 fourth\n"]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    mod fd {
        use std::ffi::OsStr;
        use std::io::Read;
        use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
        use std::time::{Duration, Instant};
        use super::super::channel::{open, parse_fd, start, writable_stream, Stream};
        use super::*;

        /// A fresh pipe as (read end, write end), both closed on drop.
        fn pipe() -> (std::fs::File, std::fs::File) {
            let mut fds: [RawFd; 2] = [0; 2];
            // SAFETY: `pipe` fills the two-element array we own.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: both fds were just created and are owned by nothing else.
            unsafe { (std::fs::File::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) }
        }

        #[test]
        fn only_bare_digits_above_stderr_name_a_channel() {
            for rejected in ["", "0", "1", "2", "-3", "+3", " 3", "3 ", "3a", "0x3", "99999999999999999999"] {
                assert_eq!(parse_fd(OsStr::new(rejected)), None, "{:?} was accepted", rejected);
            }
            assert_eq!(parse_fd(OsStr::new("3")), Some(3));
            assert_eq!(parse_fd(OsStr::new("017")), Some(17));
        }

        #[test]
        fn the_write_end_of_a_pipe_is_a_channel_and_the_read_end_is_not() {
            let (reader, writer) = pipe();
            assert_eq!(writable_stream(writer.as_raw_fd()), Some(Stream::Pipe));
            assert_eq!(writable_stream(reader.as_raw_fd()), None);
        }

        #[test]
        fn a_regular_file_or_a_closed_number_is_not_a_channel() {
            let file = regular_file();
            assert_eq!(writable_stream(file.as_raw_fd()), None);

            let (_, writer) = pipe();
            let closed = writer.as_raw_fd();
            drop(writer);
            assert_eq!(writable_stream(closed), None);
        }

        #[test]
        fn a_socket_is_a_channel() {
            let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
            assert_eq!(writable_stream(ours.as_raw_fd()), Some(Stream::Socket));
            assert!(open(OsStr::new(&ours.as_raw_fd().to_string())).is_some(), "a socket channel was refused");
        }

        #[test]
        fn opening_makes_the_fd_close_on_exec() {
            let (_reader, writer) = pipe();
            let fd = writer.as_raw_fd();
            // SAFETY: reads the descriptor flags of an fd we own.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0, "a fresh pipe was already close-on-exec");
            assert!(open(OsStr::new(&fd.to_string())).is_some());
            // SAFETY: as above.
            assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        }

        #[test]
        fn a_refused_fd_is_made_close_on_exec_too() {
            let (reader, _writer) = pipe();
            let fd = reader.as_raw_fd();
            // SAFETY: reads the descriptor flags of an fd we own.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0, "a fresh pipe was already close-on-exec");
            assert!(open(OsStr::new(&fd.to_string())).is_none(), "a read end was accepted");
            // SAFETY: as above.
            assert_ne!(unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn opening_a_socket_sets_its_no_sigpipe_option() {
            let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
            let fd = ours.as_raw_fd();

            let read_option = || {
                let mut value: libc::c_int = 0;
                let mut size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: `getsockopt` writes at most `size` bytes into `value`, which we own.
                let got = unsafe {
                    libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_NOSIGPIPE,
                                     (&mut value as *mut libc::c_int).cast(), &mut size)
                };
                assert_eq!(got, 0);
                value
            };

            assert_eq!(read_option(), 0, "a fresh socket already had it");
            assert!(open(OsStr::new(&fd.to_string())).is_some());
            assert_ne!(read_option(), 0);
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn opening_sets_the_no_sigpipe_flag_on_the_fd() {
            /// Darwin's `sys/fcntl.h` reader for the flag [`F_SETNOSIGPIPE`] sets.
            const F_GETNOSIGPIPE: libc::c_int = 74;

            let (_reader, writer) = pipe();
            let fd = writer.as_raw_fd();
            // SAFETY: reads a flag of an fd we own.
            assert_eq!(unsafe { libc::fcntl(fd, F_GETNOSIGPIPE) }, 0, "a fresh pipe already had it");
            assert!(open(OsStr::new(&fd.to_string())).is_some());
            // SAFETY: as above.
            assert_ne!(unsafe { libc::fcntl(fd, F_GETNOSIGPIPE) }, 0);
        }

        #[test]
        fn the_writer_delivers_lines_in_order() {
            let (mut reader, writer) = pipe();
            let channel = start(writer.as_raw_fd());

            channel.push(status("one"));
            channel.push(progress(Some(5), "x"));
            channel.push(status("two"));
            channel.drain(Duration::from_secs(10));
            drop(channel);
            drop(writer);

            let mut text = String::new();
            reader.read_to_string(&mut text).unwrap();
            assert!(text.starts_with("status one\n"), "{:?}", text);
            assert!(text.ends_with("status two\n"), "{:?}", text);
        }

        #[test]
        fn pushing_never_waits_on_a_reader_that_does_not_read() {
            let (_reader, writer) = pipe();
            let channel = start(writer.as_raw_fd());

            // Far more than any pipe buffer holds, alternating so nothing coalesces.
            let started = Instant::now();
            for i in 0..200_000u32 {
                channel.push(status(&format!("phase {} {}", i, "x".repeat(200))));
                channel.push(progress(Some(0), "y"));
            }
            assert!(started.elapsed() < Duration::from_secs(20), "pushes waited on the reader");

            let started = Instant::now();
            channel.drain(Duration::from_millis(100));
            assert!(started.elapsed() < Duration::from_secs(5), "drain outlived its budget");
        }

        /// An open regular file, unlinked as soon as it is open.
        fn regular_file() -> std::fs::File {
            let path = std::env::temp_dir().join(format!("forklift-events-test-{}", std::process::id()));
            let file = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(&path).unwrap();
            let _ = std::fs::remove_file(&path);
            file
        }
    }
}
