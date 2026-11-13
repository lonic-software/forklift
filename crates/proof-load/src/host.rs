//! The host a hosted scenario serves: a forklift-server over the host warehouse, reached only
//! through the network fault proxy, so every transfer of the scenario crosses a place a fault can
//! be injected, and a server the harness kills can be restarted behind the same URL.
//!
//! A restart is a harness step, and the one place the harness acts on a server's own words: when
//! the restarted server refuses to start, the remedies its refusal names are extracted in the same
//! two forms a client's are, each removal inside the run's roots is carried out and recorded as
//! outside knowledge, and anything else the refusal said is recorded as unextractable. The
//! harness never guesses at what a server left behind.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::proxy::Proxy;
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
}

/// What a restart took.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Restarted {
    /// Each removal carried out, as the step a user would have had to know to take.
    pub outside_knowledge: Vec<String>,
    /// What the refusals named that could not be carried out as written.
    pub unextractable: Vec<String>,
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Host {
    /// Serves the warehouse at `root` with `program`, behind a new proxy.
    pub fn start(program: &Path, root: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Host, ToolError> {
        let served = tool::serve(program, root, scratch, environment, limit)?;
        let upstream = address_of(&served.url).map_err(|reason| ToolError::Spawn { program: program.to_path_buf(), reason })?;
        let proxy = Proxy::start(upstream).map_err(|error| ToolError::Spawn { program: program.to_path_buf(), reason: format!("the network proxy: {}", error) })?;
        let server = Arc::new(Mutex::new(Some(served)));
        let killed = server.clone();
        proxy.set_killer(move || drop(lock(&killed).take()));
        Ok(Host { url: proxy.url.clone(), proxy, server, program: program.to_path_buf(), root: root.to_path_buf() })
    }

    /// Kills the server, as a crash would: no shutdown, whatever it holds left behind.
    pub fn kill(&self) {
        drop(lock(&self.server).take());
    }

    /// Whether the server has been killed and not restarted.
    pub fn killed(&self) -> bool {
        lock(&self.server).is_none()
    }

    /// Starts the server again over the same warehouse, behind the same URL. A refusal to start
    /// is answered with the removals it names inside the run's roots, then the server is started
    /// again; a refusal naming none ends the restart in that refusal.
    pub fn restart(&self, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Restarted, ToolError> {
        let mut restarted = Restarted::default();
        let mut refusal = None;
        for _ in 0..STARTS {
            match tool::serve(&self.program, &self.root, scratch, environment, limit) {
                Ok(served) => {
                    let upstream = address_of(&served.url).map_err(|reason| ToolError::Spawn { program: self.program.clone(), reason })?;
                    self.proxy.set_upstream(upstream);
                    *lock(&self.server) = Some(served);
                    return Ok(restarted);
                }
                Err(ToolError::NotListening { program, limit, said }) => {
                    let extracted = remedy::extract(&[said.as_str()]);
                    let mut removed = false;
                    for named in extracted.remedies {
                        match named {
                            Remedy::Remove { path } => match remedy::removable(&path, &scratch.roots()) {
                                Some(path) if path.is_file() => {
                                    #[allow(clippy::disallowed_methods)] // a removal the server's refusal named, judged inside the run's roots
                                    let gone = std::fs::remove_file(&path);
                                    if gone.is_ok() {
                                        restarted.outside_knowledge.push(format!("remove {}", path.display()));
                                        removed = true;
                                    }
                                }
                                Some(_) => {}
                                None => restarted.outside_knowledge.push(format!("remove {} (outside the run root; not run)", path.display())),
                            },
                            Remedy::Command { arguments } => restarted.unextractable.push(format!("forklift {}", arguments.join(" "))),
                        }
                    }
                    for text in extracted.unextractable {
                        if !restarted.unextractable.contains(&text) {
                            restarted.unextractable.push(text);
                        }
                    }
                    refusal = Some(ToolError::NotListening { program, limit, said });
                    if !removed {
                        break;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(refusal.expect("a restart that never started the server was refused"))
    }
}
