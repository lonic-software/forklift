//! Handing a child process a `PULT_EVENTS` channel and reading back what it wrote. Shared by
//! the integration tests that exercise the channel (`events.rs`, `remote.rs`).

use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The fd number the child finds its events channel at.
pub const CHANNEL_FD: RawFd = 7;

/// A pipe as (read end, write end), both close-on-exec — atomically where the platform allows
/// (`pipe2` on Linux; macOS has no `pipe2`, so a child spawned concurrently by another test can
/// still inherit an end, which [`run_and_collect`] tolerates).
pub fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];

    #[cfg(target_os = "linux")]
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };

    #[cfg(target_os = "macos")]
    let created = unsafe {
        let created = libc::pipe(fds.as_mut_ptr());
        if created == 0 {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        created
    };

    assert_eq!(created, 0, "pipe: {}", std::io::Error::last_os_error());

    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Run `command` with `fd` installed at [`CHANNEL_FD`] in the child and `PULT_EVENTS` naming
/// it. The parent's copy of `fd` is closed once the child has been spawned.
pub fn run_with_channel(command: &mut Command, fd: OwnedFd) -> Output {
    command.env("PULT_EVENTS", CHANNEL_FD.to_string());

    let raw = fd.into_raw_fd();
    unsafe {
        command.pre_exec(move || {
            let installed = if raw == CHANNEL_FD {
                // `dup2` onto itself is a no-op that would leave close-on-exec set.
                libc::fcntl(raw, libc::F_SETFD, 0)
            } else {
                libc::dup2(raw, CHANNEL_FD)
            };

            if installed == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        });
    }

    let output = command.output();
    drop(unsafe { OwnedFd::from_raw_fd(raw) });

    output.unwrap()
}

/// Run `command` with a fresh pipe as its channel, returning its output and everything written
/// to the channel.
///
/// Reading does not wait for end-of-file: an unrelated child that inherited the write end would
/// hold that off for as long as it lives. Once this child has exited, everything it wrote is
/// already in the pipe, so the reader stops at the first empty read after the exit.
pub fn run_and_collect(command: &mut Command) -> (Output, String) {
    let (read_end, write_end) = pipe();

    let flags = unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }, -1);

    let exited = Arc::new(AtomicBool::new(false));
    let reader_exited = Arc::clone(&exited);

    // Read while the child runs, so a chatty run never fills the pipe.
    let reader = std::thread::spawn(move || {
        let mut file = std::fs::File::from(read_end);
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];

        loop {
            // Sampled before the read: if the child had exited by now, an empty read below means
            // the pipe holds nothing more from it.
            let after_exit = reader_exited.load(Ordering::Acquire);

            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if after_exit {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("reading the channel: {}", error),
            }
        }

        String::from_utf8(bytes).expect("the channel carried invalid UTF-8")
    });

    let output = run_with_channel(command, write_end);
    exited.store(true, Ordering::Release);

    (output, reader.join().unwrap())
}
