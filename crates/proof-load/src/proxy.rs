//! The network fault proxy: a harness-owned TCP proxy on loopback between the client and a
//! forklift-server, through which every transfer of a hosted scenario runs.
//!
//! It reads each HTTP/1.1 request whole before forwarding any of it, so a fault can be placed
//! anywhere in it. Serialized (the network lane), it forwards one request at a time across every
//! connection, holding each later request until the earlier one's response has been read to its
//! end: each request's window stays apart from the next, and requests are numbered in the order
//! they reach the server. Otherwise requests on different connections are forwarded at once, as a
//! client's parallel transfers reach a server without the proxy, so every other lane still runs
//! them concurrently. Every exchange is logged: its method, its path and the status the server
//! answered, which is what route coverage reads.
//!
//! One fault can be armed at a time, at the n-th request after arming, and fires once:
//! - toward the client: delay the response, drop the request unforwarded, truncate the response,
//!   reset the connection, or stall it partway through the response;
//! - toward the server: cut the server's connection partway through the request, or kill the
//!   server once the whole request is forwarded, before its response is read.
//!
//! A fault toward the client after the request is forwarded still reads the server's response to
//! its end, so that request's window ends at the response; a fault toward the server leaves its
//! request's writes running with no response to mark their end.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a delayed response is held before it is delivered.
pub const DELAY: Duration = Duration::from_secs(3);

/// How long a stalled connection is held open, past any client read timeout, before the proxy
/// closes it.
pub const STALL_CAP: Duration = Duration::from_secs(90);

/// How long the proxy waits on the server's socket for a read or a write before giving the
/// exchange up: past every client timeout, so it ends only an exchange the client gave up on.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest request or response head the proxy reads: anything longer is not a request it
/// forwards.
const HEAD_CAP: usize = 64 * 1024;

/// A fault the proxy injects into one exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fault {
    /// The response held for [`DELAY`], then delivered whole.
    Delay,
    /// The request read whole and never forwarded; the client's connection closed.
    Drop,
    /// The response read whole, the first half of its bytes delivered, the connection closed.
    Truncate,
    /// The response read whole and none of it delivered; the client's connection reset.
    Reset,
    /// The response read whole, the first half of its bytes delivered, and the connection held
    /// open with nothing more until the client gives up or [`STALL_CAP`] passes.
    Stall,
    /// The first half of the request forwarded, then the server's connection closed; nothing
    /// delivered to the client, whose connection is closed.
    Cut,
    /// The whole request forwarded, then the server killed before its response is read; the
    /// client's connection closed.
    Kill,
}

impl Fault {
    pub const ALL: [Fault; 7] = [Fault::Delay, Fault::Drop, Fault::Truncate, Fault::Reset, Fault::Stall, Fault::Cut, Fault::Kill];

    pub fn label(self) -> &'static str {
        match self {
            Fault::Delay => "delay",
            Fault::Drop => "drop",
            Fault::Truncate => "truncate",
            Fault::Reset => "reset",
            Fault::Stall => "stall",
            Fault::Cut => "cut toward the server",
            Fault::Kill => "server killed",
        }
    }
}

/// One request the proxy read, and what came of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Exchange {
    /// Its place among every request this proxy read, from 0.
    pub index: usize,
    pub method: String,
    /// The request target's path, without its query.
    pub path: String,
    /// The status the server answered, when its response was read.
    pub status: Option<u16>,
    /// The fault injected into it, if any.
    pub fault: Option<Fault>,
}

/// The fault armed for the `at`-th request (from 0) read after arming.
#[derive(Clone, Copy, Debug)]
struct Armed {
    fault: Fault,
    at: usize,
}

struct State {
    upstream: Mutex<SocketAddr>,
    /// Whether requests are forwarded one at a time.
    serial: AtomicBool,
    /// Held, when serial, from numbering a request until its response is read.
    order: Mutex<()>,
    /// The fault armed, and the index of the first request forwarded after arming.
    armed: Mutex<Option<(Armed, usize)>>,
    log: Mutex<Vec<Exchange>>,
    /// Kills the server, for [`Fault::Kill`].
    killer: Mutex<Option<Box<dyn FnMut() + Send>>>,
    stopped: AtomicBool,
}

/// A running proxy; it stops accepting when dropped.
pub struct Proxy {
    /// The base URL clients are given: `http://127.0.0.1:<port>`.
    pub url: String,
    address: SocketAddr,
    state: Arc<State>,
    accepting: Option<std::thread::JoinHandle<()>>,
}

impl Proxy {
    /// Starts a proxy on a loopback port the system picks, forwarding to `upstream`.
    pub fn start(upstream: SocketAddr) -> std::io::Result<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let state = Arc::new(State {
            upstream: Mutex::new(upstream), serial: AtomicBool::new(false), order: Mutex::new(()), armed: Mutex::new(None),
            log: Mutex::new(Vec::new()), killer: Mutex::new(None), stopped: AtomicBool::new(false),
        });
        let accepting = {
            let state = state.clone();
            std::thread::spawn(move || {
                for client in listener.incoming() {
                    if state.stopped.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(client) = client else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || serve_connection(client, &state));
                }
            })
        };
        Ok(Proxy { url: format!("http://{}", address), address, state, accepting: Some(accepting) })
    }

    /// Points the proxy at a new server address, as a restarted server binds a new port.
    pub fn set_upstream(&self, upstream: SocketAddr) {
        *lock(&self.state.upstream) = upstream;
    }

    /// Forwards one request at a time from now on, numbered in the order they reach the server, or
    /// concurrently again.
    pub fn set_serial(&self, serial: bool) {
        self.state.serial.store(serial, Ordering::SeqCst);
    }

    /// What [`Fault::Kill`] runs to kill the server.
    pub fn set_killer(&self, killer: impl FnMut() + Send + 'static) {
        *lock(&self.state.killer) = Some(Box::new(killer));
    }

    /// How many requests the proxy has numbered so far.
    pub fn requests(&self) -> usize {
        lock(&self.state.log).len()
    }

    /// Every exchange so far, in the order the requests were numbered.
    pub fn log(&self) -> Vec<Exchange> {
        lock(&self.state.log).clone()
    }

    /// Arms `fault` for the `at`-th request (from 0) numbered from now on. It fires once.
    pub fn arm(&self, fault: Fault, at: usize) {
        let next = self.requests();
        *lock(&self.state.armed) = Some((Armed { fault, at }, next));
    }

    /// Disarms a fault that has not fired; says whether one was still armed.
    pub fn disarm(&self) -> bool {
        lock(&self.state.armed).take().is_some()
    }

    /// The exchange a fault fired on, the latest one.
    pub fn fired(&self) -> Option<Exchange> {
        lock(&self.state.log).iter().rev().find(|exchange| exchange.fault.is_some()).cloned()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
    }
}

/// A mutex's guard, whether or not a holder panicked: the proxy's state stays usable.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One HTTP/1.1 message read whole: its bytes as they came, and its first line's parts.
#[derive(Debug, PartialEq, Eq)]
struct Message {
    bytes: Vec<u8>,
    /// The request's method, or the response's version.
    first: String,
    /// The request's target, or the response's status code.
    second: String,
    /// Whether the peer closes the connection after it: a response read to the end of its
    /// connection, or one that says `Connection: close`.
    closes: bool,
}

/// How a message's body is delimited.
enum Body {
    None,
    Length(usize),
    Chunked,
    /// To the end of the connection: a response with neither length nor chunking.
    ToEnd,
}

/// A message head as read: its bytes, its headers with lowercased names, and its first line's
/// first two parts.
struct Head {
    bytes: Vec<u8>,
    headers: Vec<(String, String)>,
    first: [String; 2],
}

/// Reads one message head: None at a clean end of the connection before any byte.
fn read_head(reader: &mut impl BufRead) -> std::io::Result<Option<Head>> {
    let mut head = Vec::new();
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the connection ended inside a message head"));
        }
        head.extend_from_slice(&line);
        if head.len() > HEAD_CAP {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a message head past the proxy's cap"));
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.split("\r\n").flat_map(|line| line.split('\n'));
    let first: Vec<String> = lines.next().unwrap_or_default().splitn(3, ' ').map(str::to_string).collect();
    if first.len() < 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("not an HTTP message: {:?}", first)));
    }
    let headers = lines.filter_map(|line| line.split_once(':')).map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string())).collect();
    Ok(Some(Head { bytes: head, headers, first: [first[0].clone(), first[1].clone()] }))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

/// Reads the body `body` delimits onto `bytes`, verbatim: chunk framing included.
fn read_body(reader: &mut impl BufRead, body: Body, bytes: &mut Vec<u8>) -> std::io::Result<()> {
    match body {
        Body::None => Ok(()),
        Body::Length(length) => {
            let start = bytes.len();
            bytes.resize(start + length, 0);
            reader.read_exact(&mut bytes[start..])
        }
        Body::ToEnd => reader.read_to_end(bytes).map(|_| ()),
        Body::Chunked => loop {
            let mut size_line = Vec::new();
            if reader.read_until(b'\n', &mut size_line)? == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the connection ended inside a chunked body"));
            }
            bytes.extend_from_slice(&size_line);
            let size_text = String::from_utf8_lossy(&size_line);
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or_default().trim(), 16)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("a bad chunk size {:?}", size_text.trim())))?;
            if size == 0 {
                // Trailers, then the empty line that ends the body.
                loop {
                    let mut line = Vec::new();
                    if reader.read_until(b'\n', &mut line)? == 0 {
                        return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the connection ended inside a chunked body's trailers"));
                    }
                    bytes.extend_from_slice(&line);
                    if line == b"\r\n" || line == b"\n" {
                        return Ok(());
                    }
                }
            }
            let start = bytes.len();
            // The chunk and the line end after it.
            bytes.resize(start + size + 2, 0);
            reader.read_exact(&mut bytes[start..])?;
        },
    }
}

fn chunked(headers: &[(String, String)]) -> bool {
    header(headers, "transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
}

fn closes(headers: &[(String, String)]) -> bool {
    header(headers, "connection").is_some_and(|value| value.eq_ignore_ascii_case("close"))
}

/// Reads one request whole; None at a clean end of the connection.
fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<Message>> {
    let Some(Head { mut bytes, headers, first: [method, target] }) = read_head(reader)? else { return Ok(None) };
    let body = if chunked(&headers) {
        Body::Chunked
    } else {
        match header(&headers, "content-length").map(|length| length.parse::<usize>()) {
            Some(Ok(length)) => Body::Length(length),
            Some(Err(_)) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a bad content-length")),
            None => Body::None,
        }
    };
    read_body(reader, body, &mut bytes)?;
    Ok(Some(Message { bytes, first: method, second: target, closes: closes(&headers) }))
}

/// Reads one response whole, to the request `method` was, with any interim 1xx responses before
/// it kept in its bytes: a HEAD response, a 204 or 304 has no body whatever its headers say. None
/// when the connection ended before any byte of it.
fn read_response(reader: &mut impl BufRead, method: &str) -> std::io::Result<Option<Message>> {
    let mut bytes = Vec::new();
    let (headers, version, status, code) = loop {
        let Some(Head { bytes: head, headers, first: [version, status] }) = read_head(reader)? else {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the connection ended after an interim response"));
        };
        let code: u16 = status.parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("a bad status {:?}", status)))?;
        bytes.extend_from_slice(&head);
        // An interim response is followed by the final one on the same request; 101 switches
        // protocols, and nothing HTTP follows it.
        if !(100..200).contains(&code) || code == 101 {
            break (headers, version, status, code);
        }
    };
    let mut closing = closes(&headers);
    let body = if method == "HEAD" || (100..200).contains(&code) || code == 204 || code == 304 {
        Body::None
    } else if chunked(&headers) {
        Body::Chunked
    } else if let Some(length) = header(&headers, "content-length") {
        Body::Length(length.parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "a bad content-length"))?)
    } else {
        closing = true;
        Body::ToEnd
    };
    read_body(reader, body, &mut bytes)?;
    Ok(Some(Message { bytes, first: version, second: status, closes: closing }))
}

/// The path of a request target, without its query.
fn path_of(target: &str) -> String {
    target.split('?').next().unwrap_or_default().to_string()
}

/// A connection to the server, with a buffered reader over its read half.
struct Upstream {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Upstream {
    /// Whether the server has closed this kept connection, or sent something no request asked
    /// for: either way it is not one to send a request on. Looked at without waiting.
    fn stale(&self) -> bool {
        if !self.reader.buffer().is_empty() {
            return true;
        }
        if self.writer.set_nonblocking(true).is_err() {
            return true;
        }
        let peeked = self.writer.peek(&mut [0u8; 1]);
        let restored = self.writer.set_nonblocking(false);
        !matches!(peeked, Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock) || restored.is_err()
    }

    fn connect(address: SocketAddr) -> std::io::Result<Upstream> {
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
        stream.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
        stream.set_write_timeout(Some(UPSTREAM_TIMEOUT))?;
        Ok(Upstream { reader: BufReader::new(stream.try_clone()?), writer: stream })
    }
}

/// Resets `stream`: closed with no linger, so the peer reads a reset rather than an end. Where no
/// linger can be set without a dependency (Windows), it is closed instead.
fn reset(stream: TcpStream) {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let linger = libc::linger { l_onoff: 1, l_linger: 0 };
        // SAFETY: a valid socket descriptor, and a linger struct of the size passed.
        unsafe {
            libc::setsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_LINGER,
                (&linger as *const libc::linger).cast(), std::mem::size_of::<libc::linger>() as libc::socklen_t);
        }
    }
    drop(stream);
}

/// Takes the armed fault when the request just numbered, `index`, is the one it was armed for.
fn take_fault(state: &State, index: usize) -> Option<Fault> {
    let mut armed = lock(&state.armed);
    match *armed {
        Some((Armed { fault, at }, from)) if index == from + at => {
            *armed = None;
            Some(fault)
        }
        _ => None,
    }
}

/// Serves one client connection: each request read whole, forwarded in turn, answered.
fn serve_connection(client: TcpStream, state: &State) {
    let Ok(reading) = client.try_clone() else { return };
    let mut reader = BufReader::new(reading);
    let mut client = client;
    let mut upstream: Option<Upstream> = None;
    loop {
        let request = match read_request(&mut reader) {
            Ok(Some(request)) => request,
            _ => return,
        };
        let (index, fault, response) = {
            let _turn = state.serial.load(Ordering::SeqCst).then(|| lock(&state.order));
            let index = {
                let mut log = lock(&state.log);
                let index = log.len();
                log.push(Exchange { index, method: request.first.clone(), path: path_of(&request.second), status: None, fault: None });
                index
            };
            let fault = take_fault(state, index);
            if fault.is_some() {
                lock(&state.log)[index].fault = fault;
            }
            if fault == Some(Fault::Drop) {
                // A fault toward the client while the request is still arriving forwards nothing.
                let _ = client.shutdown(Shutdown::Both);
                return;
            }
            let address = *lock(&state.upstream);
            loop {
                // A kept connection the server has since closed (an idle close, or a close after a
                // refusal that never said so) is opened again before anything is sent on it. One
                // that fails as the request goes out is opened again, once: the server read none
                // of it. One that closes after the whole request went, before any answer, may have
                // applied it, so only a request safe to repeat is sent again; a client would not
                // resend any other. A connection just opened is not tried twice.
                if upstream.as_ref().is_some_and(Upstream::stale) {
                    upstream = None;
                }
                let reused = upstream.is_some();
                if upstream.is_none() {
                    upstream = Upstream::connect(address).ok();
                }
                let Some(server) = upstream.as_mut() else {
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                };
                if fault == Some(Fault::Cut) {
                    if server.writer.write_all(&request.bytes[..request.bytes.len() / 2]).and_then(|_| server.writer.flush()).is_err() {
                        if reused {
                            upstream = None;
                            continue;
                        }
                        let _ = client.shutdown(Shutdown::Both);
                        return;
                    }
                    let _ = server.writer.shutdown(Shutdown::Both);
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                }
                if server.writer.write_all(&request.bytes).and_then(|_| server.writer.flush()).is_err() {
                    if reused {
                        upstream = None;
                        continue;
                    }
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                }
                if fault == Some(Fault::Kill) {
                    if let Some(kill) = lock(&state.killer).as_mut() {
                        kill();
                    }
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                }
                // Unanswered is nothing read at all: the connection ended, or was reset (a server
                // that closes with the request unread in its buffer resets rather than closes).
                // A read that fails after the answer began is the server's fault to show, never
                // hidden behind a resend. Any other failure (a silent server's timeout) is final
                // here: read again, it would wait out the timeout a second time.
                let unanswered = match server.reader.fill_buf() {
                    Ok(bytes) => bytes.is_empty(),
                    Err(error) if matches!(error.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted) => true,
                    Err(_) => {
                        let _ = client.shutdown(Shutdown::Both);
                        return;
                    }
                };
                if unanswered && reused && matches!(request.first.as_str(), "GET" | "HEAD") {
                    upstream = None;
                    continue;
                }
                match read_response(&mut server.reader, &request.first) {
                    Ok(Some(response)) => break (index, fault, response),
                    _ => {
                        let _ = client.shutdown(Shutdown::Both);
                        return;
                    }
                }
            }
        };
        lock(&state.log)[index].status = response.second.parse().ok();
        if response.closes {
            upstream = None;
        }

        match fault {
            Some(Fault::Truncate) => {
                let _ = client.write_all(&response.bytes[..response.bytes.len() / 2]);
                let _ = client.shutdown(Shutdown::Both);
                return;
            }
            Some(Fault::Reset) => {
                reset(client);
                return;
            }
            Some(Fault::Stall) => {
                let _ = client.write_all(&response.bytes[..response.bytes.len() / 2]);
                let _ = client.flush();
                // Held open with nothing more: wait for the client to give up, or the cap.
                let _ = client.set_read_timeout(Some(STALL_CAP));
                let mut sink = [0u8; 4096];
                while matches!(reader.read(&mut sink), Ok(read) if read > 0) {}
                let _ = client.shutdown(Shutdown::Both);
                return;
            }
            Some(Fault::Delay) => std::thread::sleep(DELAY),
            _ => {}
        }
        if client.write_all(&response.bytes).and_then(|_| client.flush()).is_err() || request.closes || response.closes {
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A tiny HTTP/1.1 server answering each request with its own method and path as the body,
    /// counting how many requests reached it and how many it was handling at once at most.
    struct Echo {
        address: SocketAddr,
        reached: Arc<AtomicUsize>,
        widest: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    fn echo(hold: Duration) -> Echo {
        echo_closing(hold, false)
    }

    /// An [`echo`] that, with `closes`, closes each connection after its first response without
    /// saying so, as a server does after an idle while.
    fn echo_closing(hold: Duration, closes: bool) -> Echo {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reached = Arc::new(AtomicUsize::new(0));
        let widest = Arc::new(AtomicUsize::new(0));
        let busy = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (counted, wide, kept) = (reached.clone(), widest.clone(), bodies.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (counted, wide, busy, kept) = (counted.clone(), wide.clone(), busy.clone(), kept.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut stream = stream;
                    while let Ok(Some(request)) = read_request(&mut reader) {
                        let now = busy.fetch_add(1, Ordering::SeqCst) + 1;
                        wide.fetch_max(now, Ordering::SeqCst);
                        counted.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(hold);
                        let head_end = request.bytes.windows(4).position(|window| window == b"\r\n\r\n").map_or(request.bytes.len(), |at| at + 4);
                        lock(&kept).push(request.bytes[head_end..].to_vec());
                        let body = format!("{} {}", request.first, request.second);
                        let response = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}", body.len(), body);
                        busy.fetch_sub(1, Ordering::SeqCst);
                        if stream.write_all(response.as_bytes()).is_err() || closes {
                            return;
                        }
                    }
                });
            }
        });
        Echo { address, reached, widest, bodies }
    }

    /// Sends one request on a fresh connection and reads what comes back until the proxy closes
    /// or the response ends; the bytes, and whether the read ended in an error (a reset).
    fn ask(proxy: &Proxy, request: &[u8]) -> (Vec<u8>, bool) {
        let address: SocketAddr = proxy.url.trim_start_matches("http://").parse().unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        stream.write_all(request).unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            // A whole response is the answer, whatever follows it.
            if let Ok(Some(response)) = read_response(&mut std::io::Cursor::new(&bytes), "GET") {
                if !response.closes {
                    return (response.bytes, false);
                }
            }
            match stream.read(&mut buffer) {
                Ok(0) => return (bytes, false),
                Ok(read) => bytes.extend_from_slice(&buffer[..read]),
                Err(_) => return (bytes, true),
            }
        }
    }

    const GET: &[u8] = b"GET /v1/warehouse?x=1 HTTP/1.1\r\nhost: h\r\n\r\n";

    /// Requests and responses are read whole, content-length and chunked alike, and forwarded
    /// verbatim; the log names each request's method, its path without the query, and the status.
    ///
    /// Revert: forward the chunked body without its framing → the server reads a body of the
    /// wrong bytes; keep the query in the path → the logged path carries `?x=1`.
    #[test]
    fn requests_are_forwarded_whole_and_logged() {
        let server = echo(Duration::ZERO);
        let proxy = Proxy::start(server.address).unwrap();
        let (answer, _) = ask(&proxy, GET);
        assert!(answer.ends_with(b"GET /v1/warehouse?x=1"), "{}", String::from_utf8_lossy(&answer));
        let chunked = b"PUT /v1/objects/ab HTTP/1.1\r\nhost: h\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let (answer, _) = ask(&proxy, chunked);
        assert!(answer.ends_with(b"PUT /v1/objects/ab"));
        let sized = b"POST /v1/pallets/main HTTP/1.1\r\nhost: h\r\ncontent-length: 5\r\n\r\nhello";
        ask(&proxy, sized);
        assert_eq!(lock(&server.bodies)[1], b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n");
        assert_eq!(lock(&server.bodies)[2], b"hello");
        let log = proxy.log();
        assert_eq!(log.iter().map(|exchange| (exchange.method.as_str(), exchange.path.as_str(), exchange.status)).collect::<Vec<_>>(),
            vec![("GET", "/v1/warehouse", Some(200)), ("PUT", "/v1/objects/ab", Some(200)), ("POST", "/v1/pallets/main", Some(200))]);
    }

    /// Serialized, requests on concurrent connections reach the server one at a time.
    ///
    /// Revert: forward without taking the order lock → the server handles several at once.
    #[test]
    fn concurrent_requests_reach_the_server_one_at_a_time() {
        let server = echo(Duration::from_millis(100));
        let proxy = Arc::new(Proxy::start(server.address).unwrap());
        proxy.set_serial(true);
        let clients: Vec<_> = (0..6).map(|_| {
            let proxy = proxy.clone();
            std::thread::spawn(move || ask(&proxy, GET))
        }).collect();
        for client in clients {
            client.join().unwrap();
        }
        assert_eq!(server.reached.load(Ordering::SeqCst), 6);
        assert_eq!(server.widest.load(Ordering::SeqCst), 1);
    }

    /// Each fault fires once, on the request it was armed for, and does what it names: a drop
    /// and a cut deliver nothing, a cut forwards half the request, a kill runs the killer once the
    /// request is forwarded, a truncation and a stall deliver half the response, a reset ends the
    /// client's read in an error, a delay holds the whole response for its time.
    ///
    /// Revert: fire at the arming index instead of the n-th after it → the first request is
    /// faulted; forward a dropped request → the server is reached; deliver the whole response on
    /// truncation → the client reads it all.
    #[test]
    fn each_fault_fires_once_where_it_was_armed() {
        let whole = {
            let server = echo(Duration::ZERO);
            let proxy = Proxy::start(server.address).unwrap();
            ask(&proxy, GET).0
        };
        for fault in Fault::ALL {
            let server = echo(Duration::ZERO);
            let proxy = Proxy::start(server.address).unwrap();
            let killed = Arc::new(AtomicUsize::new(0));
            let kills = killed.clone();
            proxy.set_killer(move || { kills.fetch_add(1, Ordering::SeqCst); });
            ask(&proxy, GET);
            proxy.arm(fault, 1);
            assert_eq!(ask(&proxy, GET).0, whole, "{:?}: the request before the armed one is not faulted", fault);
            let started = std::time::Instant::now();
            let sized = b"POST /v1/pallets/main HTTP/1.1\r\nhost: h\r\ncontent-length: 10\r\n\r\n0123456789";
            let (answer, errored) = ask(&proxy, sized);
            let took = started.elapsed();
            let fired = proxy.fired().unwrap();
            assert_eq!((fired.index, fired.fault), (2, Some(fault)));
            assert!(!proxy.disarm(), "{:?} fired, so nothing stays armed", fault);
            let reached = server.reached.load(Ordering::SeqCst);
            match fault {
                Fault::Drop => assert_eq!((answer.len(), reached), (0, 2)),
                Fault::Cut => {
                    assert_eq!(answer.len(), 0);
                    // The server read half a request and never answered it; it was not reached whole.
                    assert_eq!(reached, 2);
                }
                Fault::Kill => assert_eq!((answer.len(), killed.load(Ordering::SeqCst)), (0, 1)),
                Fault::Truncate | Fault::Stall => {
                    let full = b"HTTP/1.1 200 OK\r\ncontent-length: 21\r\n\r\nPOST /v1/pallets/main".len();
                    assert!(!answer.is_empty() && answer.len() < full, "{:?}: {:?}", fault, String::from_utf8_lossy(&answer));
                    assert_eq!(fired.status, Some(200), "{:?}: the response was read to its end", fault);
                }
                Fault::Reset => {
                    assert!(answer.is_empty());
                    #[cfg(unix)]
                    assert!(errored, "a reset ends the read in an error");
                }
                Fault::Delay => {
                    assert!(answer.ends_with(b"POST /v1/pallets/main"));
                    assert!(took >= DELAY, "{:?}", took);
                }
            }
            if fault != Fault::Kill {
                assert_eq!(killed.load(Ordering::SeqCst), 0);
            }
            // And it fires once: the next request goes through.
            assert_eq!(ask(&proxy, GET).0, whole, "{:?}: the request after the faulted one", fault);
            let _ = errored;
        }
    }

    /// A kept server connection that the server closed without saying so is opened again for the
    /// next request on the same client connection.
    ///
    /// Revert: drop the reconnect on a reused connection → the second request's client
    /// connection is closed with no answer.
    #[test]
    fn a_server_connection_closed_unannounced_is_opened_again() {
        let server = echo_closing(Duration::ZERO, true);
        let proxy = Proxy::start(server.address).unwrap();
        let address: SocketAddr = proxy.url.trim_start_matches("http://").parse().unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..2 {
            stream.write_all(GET).unwrap();
            let response = read_response(&mut reader, "GET").unwrap().expect("an answer");
            assert!(response.bytes.ends_with(b"GET /v1/warehouse?x=1"));
        }
        assert_eq!(server.reached.load(Ordering::SeqCst), 2);
    }

    /// A kept server connection the server reset, rather than closed, after a safe-to-repeat
    /// request went out on it is opened again too: a server that closes with a request unread
    /// resets, and which one the proxy sees depends on timing.
    ///
    /// Revert: resend only on a read that ended (`Ok(None)`) → the GET is answered with nothing.
    #[cfg(unix)]
    #[test]
    fn a_server_connection_reset_under_a_safe_request_is_opened_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reached = Arc::new(AtomicUsize::new(0));
        let counted = reached.clone();
        // Answers each connection's first request, then reads the next one and resets unanswered.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if read_request(&mut reader).ok().flatten().is_none() {
                    continue;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let body = b"GET /v1/warehouse?x=1";
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body));
                if read_request(&mut reader).ok().flatten().is_some() {
                    counted.fetch_add(1, Ordering::SeqCst);
                    reset(stream);
                }
            }
        });
        let proxy = Proxy::start(address).unwrap();
        let client: SocketAddr = proxy.url.trim_start_matches("http://").parse().unwrap();
        let mut stream = TcpStream::connect(client).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        for _ in 0..2 {
            stream.write_all(GET).unwrap();
            let response = read_response(&mut reader, "GET").unwrap().expect("an answer");
            assert!(response.bytes.ends_with(b"GET /v1/warehouse?x=1"));
        }
        assert_eq!(reached.load(Ordering::SeqCst), 3, "the reset GET was sent again on a new connection");
    }

    /// An answer the server began and then cut off is the server's fault, shown to the client: the
    /// safe request is not sent again to hide it behind a clean answer.
    ///
    /// Revert: resend a GET on any failed read of a reused connection → the cut answer is replaced
    /// by a whole one from a third request.
    #[cfg(unix)]
    #[test]
    fn an_answer_cut_off_after_it_began_is_not_hidden_by_a_resend() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reached = Arc::new(AtomicUsize::new(0));
        let counted = reached.clone();
        // Answers each connection's first request whole, then begins the next answer and resets.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if read_request(&mut reader).ok().flatten().is_none() {
                    continue;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
                if read_request(&mut reader).ok().flatten().is_some() {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc").and_then(|_| stream.flush());
                    std::thread::sleep(Duration::from_millis(100));
                    reset(stream);
                }
            }
        });
        let proxy = Proxy::start(address).unwrap();
        let client: SocketAddr = proxy.url.trim_start_matches("http://").parse().unwrap();
        let mut stream = TcpStream::connect(client).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        stream.write_all(GET).unwrap();
        assert!(read_response(&mut reader, "GET").unwrap().is_some());
        stream.write_all(GET).unwrap();
        assert!(!matches!(read_response(&mut reader, "GET"), Ok(Some(_))), "the cut answer reaches the client as no whole answer");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(reached.load(Ordering::SeqCst), 2, "the GET was not sent a third time");
    }

    /// A request that went out whole on a kept connection the server then closed without answering
    /// may have been applied, so it is not sent again unless it is safe to repeat.
    ///
    /// Revert: resend whatever was answered with nothing → the POST reaches the server twice.
    #[test]
    fn a_request_the_server_may_have_applied_is_not_sent_twice() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let reached = Arc::new(AtomicUsize::new(0));
        let counted = reached.clone();
        // Answers each connection's first request, then reads the next one and closes unanswered.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if read_request(&mut reader).ok().flatten().is_none() {
                    continue;
                }
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
                if read_request(&mut reader).ok().flatten().is_some() {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let proxy = Proxy::start(address).unwrap();
        let client: SocketAddr = proxy.url.trim_start_matches("http://").parse().unwrap();
        let mut stream = TcpStream::connect(client).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        stream.write_all(GET).unwrap();
        assert!(read_response(&mut reader, "GET").unwrap().is_some());
        stream.write_all(b"POST /v1/pallets/main HTTP/1.1\r\nhost: h\r\ncontent-length: 2\r\n\r\nok").unwrap();
        assert!(!matches!(read_response(&mut reader, "POST"), Ok(Some(_))), "the POST is answered with nothing");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(reached.load(Ordering::SeqCst), 2, "the POST reached the server once");
    }

    /// An interim response is kept with the final one it precedes, and the final one's status is
    /// the response's; a connection ending before any byte is no response.
    ///
    /// Revert: take the 100 as the response → its status is read as 100 and the 200 is left over.
    #[test]
    fn interim_responses_are_read_with_the_final_one() {
        let bytes = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        let mut reader = std::io::Cursor::new(&bytes[..]);
        let response = read_response(&mut reader, "PUT").unwrap().unwrap();
        assert_eq!((response.second.as_str(), response.bytes.as_slice()), ("200", &bytes[..]));
        assert_eq!(read_response(&mut std::io::Cursor::new(&b""[..]), "GET").unwrap(), None);
    }

    /// A restarted server is reached at its new address.
    ///
    /// Revert: ignore `set_upstream` → the request goes to the old address, which answers nothing.
    #[test]
    fn a_new_upstream_is_used_from_the_next_connection() {
        let first = echo(Duration::ZERO);
        let proxy = Proxy::start(first.address).unwrap();
        ask(&proxy, GET);
        let second = echo(Duration::ZERO);
        proxy.set_upstream(second.address);
        ask(&proxy, GET);
        assert_eq!((first.reached.load(Ordering::SeqCst), second.reached.load(Ordering::SeqCst)), (1, 1));
    }
}
