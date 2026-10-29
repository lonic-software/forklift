//! The CLI's fetch hook for a cache warehouse's reads (see `forklift_core::util::cache_miss`): an
//! object a read lacks is fetched from the warehouse's origin.
//!
//! The configuration is read on the reading thread, which resolves the warehouse the read is in.
//! The request runs on a runtime of the hook's own, started on first use, and the reading thread
//! waits for its reply: a read is synchronous, and may run on a runtime thread that must not start
//! another runtime inside its own. Requests from several reading threads run at once.

use std::sync::mpsc;
use std::sync::OnceLock;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::cache_miss::{self, Fetched};
use forklift_core::util::config_utils;
use forklift_core::util::remote_utils::{ObjectFetch, RemoteClient, TorMode, TorSettings};

/// One fetch, sent to the fetch thread.
struct Request {
    url: String,
    token: Option<String>,
    tor: TorSettings,
    hash: String,
    reply: mpsc::Sender<Fetched>,
}

/// Register the hook, for the rest of this process.
pub fn install() {
    cache_miss::register(Box::new(fetch));
}

/// Fetch `hash` from the origin of the warehouse this thread reads.
fn fetch(hash: &str) -> Fetched {
    let (url, token, tor) = match origin_settings() {
        Ok(settings) => settings,
        Err(why) => return Fetched::Failed(why),
    };

    let (reply, replied) = mpsc::channel();
    let sent = fetch_thread().send(Request {
        url,
        token,
        tor,
        hash: hash.to_string(),
        reply,
    });

    if sent.is_err() {
        return Fetched::Failed("the fetch thread has stopped".to_string());
    }

    replied.recv().unwrap_or_else(|_| Fetched::Failed("the fetch thread stopped before it replied".to_string()))
}

/// The origin's URL, the token and the Tor settings to fetch with. The configured remote must be
/// the origin: only the origin was proved to hold what this warehouse holds by hash.
fn origin_settings() -> Result<(String, Option<String>, TorSettings), String> {
    let origin = config_utils::get_scoped_value(config_utils::KEY_REMOTE_ORIGIN, ConfigScope::Warehouse)?
        .ok_or("this warehouse's origin is not recorded".to_string())?;

    let url = config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.map(|(value, _)| value);

    if url.as_deref() != Some(origin.as_str()) {
        return Err(format!(
            "only its origin ({}) holds it, and the configured remote is {}; set it back with \
            \"config {} {}\"",
            origin,
            url.map(|url| format!("\"{}\"", url)).unwrap_or_else(|| "unset".to_string()),
            config_utils::KEY_REMOTE_URL,
            origin
        ));
    }

    let token = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOKEN)?.map(|(value, _)| value);

    Ok((origin, token, TorSettings::from_config()?))
}

/// The sending end of the fetch thread's queue, which starts it on first use.
fn fetch_thread() -> &'static mpsc::Sender<Request> {
    static SENDER: OnceLock<mpsc::Sender<Request>> = OnceLock::new();

    SENDER.get_or_init(|| {
        let (sender, requests) = mpsc::channel::<Request>();

        let spawned = std::thread::Builder::new()
            .name("forklift-cache-fetch".to_string())
            .spawn(move || serve(requests));

        // A thread that could not start drops the receiver, so every send reports it.
        drop(spawned);

        sender
    })
}

/// Start each request on the runtime as it arrives, until every sender is gone, keeping the
/// client of the last origin asked.
fn serve(requests: mpsc::Receiver<Request>) {
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            for request in requests {
                let _ = request.reply.send(Fetched::Failed(format!("could not start the fetch runtime: {}", e)));
            }

            return;
        }
    };

    let mut client: Option<((String, Option<String>, TorMode, String), RemoteClient)> = None;

    for request in requests {
        let key = (request.url.clone(), request.token.clone(), request.tor.mode.clone(), request.tor.proxy.clone());

        if client.as_ref().is_none_or(|(held, _)| *held != key) {
            match RemoteClient::new_with_tor(&request.url, request.token.clone(), request.tor.clone()) {
                Ok(built) => client = Some((key, built)),
                Err(why) => {
                    let _ = request.reply.send(Fetched::Failed(why));
                    continue;
                }
            }
        }

        let (_, remote) = client.as_ref().expect("a client was just kept");
        let remote = remote.clone();

        runtime.spawn(async move {
            let fetched = match remote.fetch_object_classified(&request.hash).await {
                ObjectFetch::Fetched(bytes) => Fetched::Bytes(bytes),
                ObjectFetch::Unreachable(why) => Fetched::Unreachable { origin: request.url, why },
                ObjectFetch::Refused(why) => Fetched::Failed(why),
            };

            let _ = request.reply.send(fetched);
        });
    }
}
