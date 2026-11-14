//! The host a hosted scenario serves: a forklift-server over the host warehouse, reached only
//! through the network fault proxy, so every transfer of the scenario crosses a place a fault can
//! be injected, and a server the harness kills can be restarted behind the same URL.
//!
//! A restart is a harness step, and the one place the harness acts on a server's own words: when
//! the restarted server refuses to start, the remedies its refusal names are extracted in the same
//! two forms a client's are, each removal inside the run's roots is carried out and recorded as
//! outside knowledge, and anything else the refusal said is recorded as unextractable. The
//! harness never guesses at what a server left behind.
//!
//! A traced host is served untraced until it is listening, then strace is attached to the running
//! server, so its startup never counts toward a trace. The proxy forwards one request at a time
//! then, so each request's window holds only the writes its own handler, or the server's
//! background work, made.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::proxy::{lock, Exchange, Proxy};
use crate::remedy::{self, Remedy};
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Served, ToolError};

/// How many times a restart starts the server, each after carrying out the removals the last
/// refusal named, before it gives up.
const STARTS: usize = 8;

/// A served host warehouse behind the proxy; the server is killed when this is dropped.
pub struct Host {
    /// The base URL clients are given: the proxy's.
    pub url: String,
    pub proxy: Proxy,
    server: Arc<Mutex<Option<Served>>>,
    program: PathBuf,
    root: PathBuf,
    tracing: Mutex<Option<Tracing>>,
}

/// strace attached to one running server.
struct Attached {
    child: Child,
    trace: PathBuf,
    said: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How a traced host traces each server it starts: strace, by absolute path, attached to each in
/// turn with `filter` added to its line, the n-th writing `<base>-<n>.trace`. A tracer that is not
/// `again` traces only the server serving when it was attached: one that injects a kill must not
/// kill the server restarted after it.
struct Tracing {
    strace: PathBuf,
    base: PathBuf,
    filter: Vec<String>,
    again: bool,
    attached: Vec<Attached>,
}

/// Whether every thread of process `pid` has a tracer, as its `/proc` status says.
fn traced(pid: u32) -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{}/task", pid)) else { return false };
    let mut any = false;
    for task in tasks.flatten() {
        // A thread that ended since the folder was listed has no status left to read.
        let Ok(status) = std::fs::read_to_string(task.path().join("status")) else { continue };
        any = true;
        let tracer = status.lines().find_map(|line| line.strip_prefix("TracerPid:")).map(str::trim);
        if tracer.is_none_or(|tracer| tracer == "0") {
            return false;
        }
    }
    any
}

/// Attaches `strace` to the running process `pid`, following every thread it has and makes, its
/// trace written to `trace`; returns once every thread the process has shows a tracer. What
/// strace said is the error when it exits first or does not attach within `limit`.
fn attach(strace: &Path, pid: u32, trace: &Path, filter: &[String], limit: Duration) -> Result<Attached, String> {
    let mut child = Command::new(strace).args(["-f", "-qq", "-y", "-ttt", "-o"]).arg(trace).args(filter).arg("-p").arg(pid.to_string())
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn()
        .map_err(|error| format!("strace could not be started: {}", error))?;
    let said = tool::drain(child.stderr.take().expect("stderr is piped"));
    let mut attached = Attached { child, trace: trace.to_path_buf(), said: Some(said) };
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(Some(status)) = attached.child.try_wait() {
            let said = attached.said.take().map(|said| said.join().unwrap_or_default()).unwrap_or_default();
            return Err(format!("strace exited ({}) attaching to the server: {}", status, String::from_utf8_lossy(&said).trim()));
        }
        if traced(pid) {
            return Ok(attached);
        }
        if Instant::now() >= deadline {
            let _ = attached.child.kill();
            let _ = attached.child.wait();
            let said = attached.said.take().map(|said| said.join().unwrap_or_default()).unwrap_or_default();
            return Err(format!("strace had not attached to every thread of the server within {:?}: {}", limit, String::from_utf8_lossy(&said).trim()));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// How long a tracer gets to finish its trace once its server is killed.
const TRACE_END: Duration = Duration::from_secs(30);

/// What a restart took, and whether the server came back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restarted {
    /// Each removal the refusals named, as the step a user would have had to know to take, with
    /// why when it was not carried out.
    pub outside_knowledge: Vec<String>,
    /// What the refusals named that could not be carried out as written.
    pub unextractable: Vec<String>,
    /// What the server last said, when it would not start again: it serves nothing from then on.
    pub refused: Option<String>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").field("url", &self.url).field("root", &self.root).finish()
    }
}

/// The address a server's printed URL names: `http://<address>`.
fn address_of(url: &str) -> Result<SocketAddr, String> {
    url.trim_start_matches("http://").trim_end_matches('/').parse().map_err(|_| format!("the server printed {:?}, not an http://<address> URL", url))
}

impl Host {
    /// Serves the warehouse at `root` with `program`, behind a new proxy.
    pub fn start(program: &Path, root: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Host, ToolError> {
        let served = tool::serve(program, root, scratch, environment, limit)?;
        let upstream = address_of(&served.url).map_err(|reason| ToolError::Spawn { program: program.to_path_buf(), reason })?;
        let proxy = Proxy::start(upstream).map_err(|error| ToolError::Spawn { program: program.to_path_buf(), reason: format!("the network proxy: {}", error) })?;
        let server = Arc::new(Mutex::new(Some(served)));
        // Weak: a proxy connection thread outliving the host must not keep its server alive.
        let killed: Weak<Mutex<Option<Served>>> = Arc::downgrade(&server);
        proxy.set_killer(move || if let Some(server) = killed.upgrade() { drop(lock(&server).take()) });
        Ok(Host { url: proxy.url.clone(), proxy, server, program: program.to_path_buf(), root: root.to_path_buf(), tracing: Mutex::new(None) })
    }

    /// [`Host::start`], with `strace` attached to the server once it is listening, its trace
    /// written beside `base`, and the proxy forwarding one request at a time. When strace cannot
    /// attach, the host serves untraced and the second value is what strace said: the harness never
    /// falls back to a trace it did not take.
    pub fn start_traced(program: &Path, root: &Path, scratch: &Scratch, environment: &Environment, limit: Duration, strace: &Path, base: &Path) -> Result<(Host, Option<String>), ToolError> {
        let host = Host::start(program, root, scratch, environment, limit)?;
        host.proxy.set_serial(true);
        *lock(&host.tracing) = Some(Tracing { strace: strace.to_path_buf(), base: base.to_path_buf(), filter: Vec::new(), again: true, attached: Vec::new() });
        let failed = host.attach(limit).err();
        if failed.is_some() {
            *lock(&host.tracing) = None;
        }
        Ok((host, failed))
    }

    /// Attaches strace, with `filter` added to its line, to the server serving now, and only to
    /// it: a server restarted later serves untraced. Its trace is written to `<base>-0.trace` and
    /// read by [`Host::finish`]. For a host that is not traced already; the proxy forwards one
    /// request at a time from here on, as a traced host's does.
    pub fn trace_once(&self, strace: &Path, base: &Path, filter: &[String], limit: Duration) -> Result<(), String> {
        {
            let mut tracing = lock(&self.tracing);
            if tracing.is_some() {
                return Err("the host is traced already".to_string());
            }
            *tracing = Some(Tracing { strace: strace.to_path_buf(), base: base.to_path_buf(), filter: filter.to_vec(), again: false, attached: Vec::new() });
        }
        self.proxy.set_serial(true);
        self.attach(limit)
    }

    /// Attaches the host's tracer, when it has one that traces this server, to the server now
    /// serving.
    fn attach(&self, limit: Duration) -> Result<(), String> {
        let mut tracing = lock(&self.tracing);
        let Some(tracing) = tracing.as_mut() else { return Ok(()) };
        if !tracing.again && !tracing.attached.is_empty() {
            return Ok(());
        }
        let Some(pid) = lock(&self.server).as_ref().map(Served::pid) else { return Err("the server is not running".to_string()) };
        let trace = PathBuf::from(format!("{}-{}.trace", tracing.base.display(), tracing.attached.len()));
        tracing.attached.push(attach(&tracing.strace, pid, &trace, &tracing.filter, limit)?);
        Ok(())
    }

    /// The process id of the server serving now, if one is.
    pub fn pid(&self) -> Option<u32> {
        lock(&self.server).as_ref().map(Served::pid)
    }

    /// Whether the host's servers are traced.
    pub fn is_traced(&self) -> bool {
        lock(&self.tracing).is_some()
    }

    /// Kills the server and ends its traces: everything the proxy logged, and the text of each
    /// trace in the order its server started, or why it cannot be read. Each trace file is removed
    /// once read.
    pub fn finish(self) -> (Vec<Exchange>, Vec<Result<String, String>>) {
        self.kill();
        let log = self.proxy.log();
        let attached = lock(&self.tracing).take().map(|tracing| tracing.attached).unwrap_or_default();
        let mut traces = Vec::new();
        for mut tracer in attached {
            let deadline = Instant::now() + TRACE_END;
            let ended = loop {
                match tracer.child.try_wait() {
                    Ok(Some(_)) => break true,
                    Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                    _ => break false,
                }
            };
            if !ended {
                traces.push(Err(format!("strace did not end within {:?} of its server's kill", TRACE_END)));
                continue;
            }
            traces.push(std::fs::read_to_string(&tracer.trace).map_err(|error| format!("{}: {}", tracer.trace.display(), error)));
            #[allow(clippy::disallowed_methods, reason = "the harness's own trace, read in full above")]
            let _ = std::fs::remove_file(&tracer.trace);
        }
        (log, traces)
    }

    /// Kills the server, as a crash would: no shutdown, whatever it holds left behind.
    pub fn kill(&self) {
        drop(lock(&self.server).take());
    }

    /// Whether the server has been killed and not restarted: by the harness, or by anything else
    /// that ended its process, such as an injected kill.
    pub fn killed(&self) -> bool {
        let mut server = lock(&self.server);
        if server.as_mut().is_some_and(Served::exited) {
            *server = None;
        }
        server.is_none()
    }

    /// Starts the server again over the same warehouse, behind the same URL. A refusal to start
    /// is answered with the removals it names inside the run's roots, then the server is started
    /// again; a refusal after which nothing was removed ends the restart, the server down and the
    /// refusal kept. An error is the harness failing to run the server at all.
    pub fn restart(&self, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Restarted, ToolError> {
        let mut restarted = Restarted::default();
        for _ in 0..STARTS {
            match tool::serve(&self.program, &self.root, scratch, environment, limit) {
                Ok(served) => {
                    let upstream = address_of(&served.url).map_err(|reason| ToolError::Spawn { program: self.program.clone(), reason })?;
                    *lock(&self.server) = Some(served);
                    restarted.refused = None;
                    // A host traced on every start traces its restarted server before the proxy
                    // reaches it; one that cannot be is killed and an error, never a server
                    // serving untraced into a traced run. A tracer attached once does not follow.
                    if let Err(reason) = self.attach(limit) {
                        self.kill();
                        return Err(ToolError::Spawn { program: self.program.clone(), reason });
                    }
                    self.proxy.set_upstream(upstream);
                    return Ok(restarted);
                }
                Err(ToolError::NotListening { said, .. }) => {
                    let extracted = remedy::extract(&[said.as_str()]);
                    let mut removed = false;
                    for named in extracted.remedies {
                        match named {
                            Remedy::Remove { path } => {
                                let (step, done) = remedy::remove(&path, &scratch.roots());
                                restarted.outside_knowledge.push(step);
                                removed |= done;
                            }
                            Remedy::Command { arguments } => restarted.unextractable.push(format!("forklift {}", arguments.join(" "))),
                        }
                    }
                    for text in extracted.unextractable {
                        if !restarted.unextractable.contains(&text) {
                            restarted.unextractable.push(text);
                        }
                    }
                    restarted.refused = Some(said);
                    if !removed {
                        break;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(restarted)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Killed here, not when the last handle to it goes: a proxy thread may outlive the host.
        self.kill();
    }
}
