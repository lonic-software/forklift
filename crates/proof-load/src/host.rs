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
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::proxy::{lock, Proxy};
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
    /// again; a refusal after which nothing was removed ends the restart, the server down and the
    /// refusal kept. An error is the harness failing to run the server at all.
    pub fn restart(&self, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Restarted, ToolError> {
        let mut restarted = Restarted::default();
        for _ in 0..STARTS {
            match tool::serve(&self.program, &self.root, scratch, environment, limit) {
                Ok(served) => {
                    let upstream = address_of(&served.url).map_err(|reason| ToolError::Spawn { program: self.program.clone(), reason })?;
                    self.proxy.set_upstream(upstream);
                    *lock(&self.server) = Some(served);
                    restarted.refused = None;
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
