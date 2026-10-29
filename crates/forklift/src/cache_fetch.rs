//! The CLI's fetch hook for a cache warehouse's reads (see `forklift_core::util::cache_miss`): the
//! objects a read or a presence phase lacks are fetched from the warehouse's origin, many in one
//! batch where the origin serves one, and each of the rest on its own.
//!
//! The configuration is read on the reading thread, which resolves the warehouse the read is in.
//! The request runs on a runtime of the hook's own, started on first use, and the reading thread
//! hands on each object as it arrives: a read is synchronous, and may run on a runtime thread that must not start
//! another runtime inside its own. Requests from several reading threads run at once.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::model::remote::MAX_MISSING_BATCH;
use forklift_core::util::cache_miss::{self, Deliver, Fetched};
use forklift_core::util::config_utils;
use forklift_core::util::remote_utils::{BatchOutcome, ObjectFetch, RemoteClient, TorMode, TorSettings};

/// How many objects are fetched on their own at once.
const LOOSE_FETCHES: usize = 8;

/// How many fetched objects wait for the reading thread before the fetch does. An object may be
/// as large as the store admits, so this is kept small: what the fetch holds beyond the reading
/// thread's group is these, plus the one a batch is reading, or the [`LOOSE_FETCHES`] fetched on
/// their own.
const QUEUED_OBJECTS: usize = 2;

/// One fetch, sent to the fetch thread.
struct Request {
    url: String,
    token: Option<String>,
    tor: TorSettings,
    hashes: Vec<String>,
    reply: mpsc::SyncSender<Reply>,
}

/// What the fetch thread sends back: each object as it arrives, then how the fetch ended. A
/// reply that cannot be sent, because the reading thread stopped taking them, stops the fetch.
enum Reply {
    Object(String, Vec<u8>),
    End(Fetched),
}

/// Register the hook, for the rest of this process.
pub fn install() {
    cache_miss::register(Box::new(fetch));
}

/// Fetch `hashes` from the origin of the warehouse this thread reads, delivering each object here.
fn fetch(hashes: &[String], deliver: Deliver) -> Fetched {
    let (url, token, tor) = match origin_settings() {
        Ok(settings) => settings,
        Err(why) => return Fetched::Failed(why),
    };

    let (reply, replies) = mpsc::sync_channel(QUEUED_OBJECTS);
    let sent = fetch_thread().send(Request {
        url,
        token,
        tor,
        hashes: hashes.to_vec(),
        reply,
    });

    if sent.is_err() {
        return Fetched::Failed("the fetch thread has stopped".to_string());
    }

    // Returning drops `replies`, which stops the fetch at its next object.
    for reply in replies {
        match reply {
            Reply::Object(hash, bytes) => {
                if let Err(why) = deliver(hash, bytes) {
                    return Fetched::Failed(why);
                }
            }
            Reply::End(fetched) => return fetched,
        }
    }

    Fetched::Failed("the fetch thread stopped before it replied".to_string())
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
                let _ = request.reply.send(Reply::End(Fetched::Failed(format!("could not start the fetch runtime: {}", e))));
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
                    let _ = request.reply.send(Reply::End(Fetched::Failed(why)));
                    continue;
                }
            }
        }

        let (_, remote) = client.as_ref().expect("a client was just kept");
        let remote = remote.clone();

        runtime.spawn(async move {
            let end = fetch_all(&remote, &request.url, request.hashes, &request.reply).await;
            let _ = tokio::task::block_in_place(move || request.reply.send(Reply::End(end)));
        });
    }
}

/// Fetch `hashes` from `remote`, sending each object to `reply`: in batches of at most what a head
/// answers while the origin serves them and more than one is left, then each of the rest on its
/// own. A failed batch ends the batches, and an origin that cannot be reached ends the rest, so a
/// reading thread offline learns so from a batch and the first fetches on their own, not one
/// request per object.
async fn fetch_all(remote: &RemoteClient, origin: &str, hashes: Vec<String>, reply: &mpsc::SyncSender<Reply>) -> Fetched {
    let sent: Arc<Mutex<HashSet<String>>> = Default::default();
    let stopped = Arc::new(AtomicBool::new(false));
    let mut pending = hashes;
    let mut loose: Vec<String> = Vec::new();

    while pending.len() > 1 && remote.batch_lane_open() {
        let rest = pending.split_off(pending.len().min(MAX_MISSING_BATCH));
        let batch = std::mem::replace(&mut pending, rest);
        let (to, sent_here, stopped_here) = (reply.clone(), sent.clone(), stopped.clone());

        let outcome = remote.fetch_batch_delivering(&batch, move |hash, bytes| {
            sent_here.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(hash.clone());
            to.send(Reply::Object(hash, bytes)).map_err(|_| {
                stopped_here.store(true, Ordering::SeqCst);
                "the read stopped taking objects".to_string()
            })
        }).await;

        if stopped.load(Ordering::SeqCst) {
            return Fetched::Failed("the read stopped taking objects".to_string());
        }

        let before = batch.len();
        let left = unsent(batch, &sent);

        match outcome {
            // A remote cut at its byte budget sent what fit: ask again for the rest, while it
            // sends something.
            Ok(BatchOutcome::Partial(_)) if left.len() < before => pending.splice(0..0, left).for_each(drop),
            // What this batch did not bring is fetched on its own below; the next batch is asked
            // for only if this outcome left the lane open.
            Ok(_) => loose.extend(left),
            // An error says nothing this side can classify, and may be the origin out of reach:
            // everything left is fetched on its own, whose first fetch says which.
            Err(_) => {
                loose.extend(left);
                break;
            }
        }
    }

    loose.append(&mut pending);

    let mut fetches = tokio::task::JoinSet::new();
    let mut left = loose.into_iter();

    loop {
        while fetches.len() < LOOSE_FETCHES {
            let Some(hash) = left.next() else { break };
            let remote = remote.clone();

            fetches.spawn(async move {
                let fetched = remote.fetch_object_classified(&hash).await;
                (hash, fetched)
            });
        }

        let Some(joined) = fetches.join_next().await else { return Fetched::Done };

        let (hash, fetched) = match joined {
            Ok(done) => done,
            Err(e) => return Fetched::Failed(format!("a fetch stopped: {}", e)),
        };

        match fetched {
            ObjectFetch::Fetched(bytes) => {
                let to = reply.clone();
                let sent = tokio::task::block_in_place(move || to.send(Reply::Object(hash, bytes)));

                if sent.is_err() {
                    return Fetched::Failed("the read stopped taking objects".to_string());
                }
            }
            // Dropping `fetches` stops the rest.
            ObjectFetch::Unreachable(why) => return Fetched::Unreachable { origin: origin.to_string(), why },
            ObjectFetch::Refused(why) => return Fetched::Failed(why),
        }
    }
}

/// Those of `hashes` not sent yet.
fn unsent(hashes: Vec<String>, sent: &Mutex<HashSet<String>>) -> Vec<String> {
    let sent = sent.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    hashes.into_iter().filter(|hash| !sent.contains(hash)).collect()
}
