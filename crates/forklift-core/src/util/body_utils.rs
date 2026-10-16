//! Capped reads of HTTP response bodies — the one way any production code reads a response
//! (FORK-80). A remote, or a hook endpoint, decides how many bytes it sends; reqwest's own
//! `bytes()`/`json()`/`text()` read whatever arrives, so a hostile or broken peer could make any
//! of those calls hold an arbitrary amount of memory. Every reader here takes a cap derived from
//! what the request can legitimately produce and refuses the moment the body would pass it.
//!
//! The enforcer is `clippy.toml`: it disallows reqwest's whole-body readers and its `chunk` /
//! `bytes_stream`, so a new uncapped read fails `cargo clippy` in CI. The only allowed `chunk`
//! calls are the loop below and the streaming loops that hand bytes to disk or to the bundle
//! importer (each carries its own justification). One way around the lint remains and is not
//! machine-caught: converting a `reqwest::Response` into a generic body (`reqwest::Body`,
//! `http::Response`) and collecting that. Nothing does so; review is the guard for it.
//!
//! The cap counts *decoded* bytes: transport decompression happens below `chunk`. That is only a
//! memory bound because the client accepts gzip alone, whose decoder window is a fixed 32 KiB —
//! zstd's would let a hostile frame make the decoder itself hold up to 128 MiB beneath any cap,
//! which is why the workspace's reqwest dependency does not enable its `zstd` feature.

use serde::de::DeserializeOwned;

/// The cap for a response that carries only a status-shaped JSON error body, a signature
/// sidecar, or a hook's small answer.
pub const SMALL_BODY_CAP: u64 = 64 * 1024;

/// The cap for an answer that lists up to `items` entries of at most `per_item` bytes each, plus
/// [`SMALL_BODY_CAP`] for its envelope. Saturates rather than overflowing.
pub fn per_item_body_cap(items: usize, per_item: u64) -> u64 {
    (items as u64).saturating_mul(per_item).saturating_add(SMALL_BODY_CAP)
}

/// Why a capped read did not return a body.
#[derive(Debug)]
pub enum BodyReadError {
    /// The transport failed while the body was being read.
    Transport(reqwest::Error),

    /// The body (as declared by its `Content-Length`, or as actually sent) is larger than `cap`.
    OverCap { cap: u64 },
}

/// Why a capped JSON read did not return a value.
#[derive(Debug)]
pub enum JsonReadError {
    /// Reading the body failed.
    Body(BodyReadError),

    /// The body was read but is not the expected JSON.
    Parse(serde_json::Error),
}

/// The user-facing refusal for an over-cap body, naming what was being read and the cap.
pub fn over_cap_message(what: &str, cap: u64) -> String {
    format!(
        "The answer to {} is larger than the {} bytes it can legitimately be; refusing to read \
        the rest of it.",
        what, cap
    )
}

/// Read a whole response body, refusing one larger than `cap` bytes.
///
/// A declared `Content-Length` over the cap is refused before any of the body is read. A declared
/// length is never used to preallocate: a peer can lie about it, so the buffer only ever grows
/// with the bytes that actually arrive, and a body that runs past the cap (whatever it declared)
/// is refused as soon as the chunk that would cross it arrives.
///
/// # Arguments
/// * `response` - The response whose body to read.
/// * `cap`      - The largest body accepted, in decoded bytes.
///
/// # Returns
/// * `Ok(Vec<u8>)`          - The whole body, at most `cap` bytes.
/// * `Err(BodyReadError)`   - A transport failure, or a body over the cap.
#[allow(clippy::disallowed_methods)] // this is the capped reader every other read goes through
pub async fn read_body_capped(mut response: reqwest::Response, cap: u64) -> Result<Vec<u8>, BodyReadError> {
    if response.content_length().is_some_and(|declared| declared > cap) {
        return Err(BodyReadError::OverCap { cap });
    }

    let mut body: Vec<u8> = Vec::new();

    while let Some(chunk) = response.chunk().await.map_err(BodyReadError::Transport)? {
        if body.len() as u64 + chunk.len() as u64 > cap {
            return Err(BodyReadError::OverCap { cap });
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// Read a response body capped at `cap` bytes and parse it as JSON.
///
/// # Arguments
/// * `response` - The response whose body to read.
/// * `cap`      - The largest body accepted, in decoded bytes.
///
/// # Returns
/// * `Ok(T)`                - The parsed value.
/// * `Err(JsonReadError)`   - The body could not be read within the cap, or is not a `T`.
pub async fn read_json_capped<T: DeserializeOwned>(response: reqwest::Response, cap: u64) -> Result<T, JsonReadError> {
    let body = read_body_capped(response, cap).await.map_err(JsonReadError::Body)?;

    serde_json::from_slice(&body).map_err(JsonReadError::Parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    /// Serve one connection: read the request head, write `head` then `body`, then hold the
    /// socket open for `stall_for` (if any) before closing it.
    fn serve_once(head: String, body: Vec<u8>, stall_for: Option<Duration>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();

            if let Some(delay) = stall_for {
                std::thread::sleep(delay);
            }
        });

        url
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn read(url: &str, cap: u64) -> Result<Vec<u8>, BodyReadError> {
        runtime().block_on(async {
            let response = reqwest::Client::new().get(url).send().await.unwrap();
            read_body_capped(response, cap).await
        })
    }

    fn length_head(declared: usize) -> String {
        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", declared)
    }

    const CLOSE_DELIMITED_HEAD: &str = "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n";
    const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";

    fn chunked(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();

        for piece in body.chunks(1000) {
            out.extend(format!("{:x}\r\n", piece.len()).as_bytes());
            out.extend(piece);
            out.extend(b"\r\n");
        }

        out.extend(b"0\r\n\r\n");
        out
    }

    /// The three framings a body can arrive in: a declared length, none (close-delimited), and
    /// chunked.
    fn every_framing(body: &[u8]) -> [String; 3] {
        [
            serve_once(length_head(body.len()), body.to_vec(), None),
            serve_once(CLOSE_DELIMITED_HEAD.to_string(), body.to_vec(), None),
            serve_once(CHUNKED_HEAD.to_string(), chunked(body), None),
        ]
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_read_whatever_its_framing() {
        let body = vec![7u8; 5000];

        for url in every_framing(&body) {
            assert_eq!(read(&url, 5000).unwrap(), body);
        }
    }

    #[test]
    fn a_body_one_byte_over_the_cap_is_refused_whatever_its_framing() {
        let body = vec![7u8; 5001];

        for url in every_framing(&body) {
            assert!(matches!(read(&url, 5000), Err(BodyReadError::OverCap { cap: 5000 })));
        }
    }

    #[test]
    fn a_length_declared_under_the_cap_does_not_license_reading_past_it() {
        // The peer declares 100 bytes and sends 200,000. The body is the declared 100; nothing
        // past the declaration is read into it, so a small declaration is no way around the cap.
        let url = serve_once(length_head(100), vec![1u8; 200_000], None);
        assert_eq!(read(&url, 1000).unwrap().len(), 100);
    }

    #[test]
    fn an_over_cap_declared_length_is_refused_before_the_body_arrives() {
        // The peer declares 10 MB, sends nothing and holds the connection open. A reader that
        // waited for the body would block for the whole stall.
        let url = serve_once(length_head(10_000_000), Vec::new(), Some(Duration::from_secs(30)));
        let started = Instant::now();

        assert!(matches!(read(&url, 1000), Err(BodyReadError::OverCap { cap: 1000 })));
        assert!(started.elapsed() < Duration::from_secs(10),
            "an over-declared length must be refused without reading: took {:?}", started.elapsed());
    }

    #[test]
    fn a_gzip_body_is_capped_on_its_decoded_size() {
        // A megabyte of zeros compresses to about a kilobyte: the encoded body is far under the
        // cap and the decoded one far over it. The cap counts what the caller would hold.
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&vec![0u8; 1_000_000]).unwrap();
        let encoded = encoder.finish().unwrap();
        assert!(encoded.len() < 100_000);

        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            encoded.len()
        );
        let url = serve_once(head, encoded, None);

        assert!(matches!(read(&url, 100_000), Err(BodyReadError::OverCap { cap: 100_000 })));
    }

    #[test]
    fn the_client_does_not_advertise_zstd() {
        // reqwest advertises exactly the encodings it decodes. zstd's decoder window (up to
        // 128 MiB per response under a hostile frame) sits beneath every cap here, so the client
        // must never accept it; re-enabling reqwest's `zstd` feature turns this red.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap();
            let _ = stream.write_all(length_head(0).as_bytes());
            String::from_utf8_lossy(&request[..n]).to_lowercase()
        });

        let _ = runtime().block_on(async { reqwest::Client::new().get(&url).send().await });
        let request = seen.join().unwrap();
        let accept = request.lines().find(|line| line.starts_with("accept-encoding:"))
            .expect("the client advertises its encodings").to_string();

        assert!(accept.contains("gzip"), "gzip stays accepted: {}", accept);
        assert!(!accept.contains("zstd"), "zstd must not be accepted: {}", accept);
    }

    #[test]
    fn json_is_parsed_from_the_capped_bytes_only() {
        let parsed: serde_json::Value = runtime().block_on(async {
            let url = serve_once(length_head(13), br#"{"error":"x"}"#.to_vec(), None);
            let response = reqwest::Client::new().get(&url).send().await.unwrap();
            read_json_capped(response, 13).await
        }).unwrap();
        assert_eq!(parsed["error"], "x");

        let refused = runtime().block_on(async {
            let url = serve_once(length_head(13), br#"{"error":"x"}"#.to_vec(), None);
            let response = reqwest::Client::new().get(&url).send().await.unwrap();
            read_json_capped::<serde_json::Value>(response, 12).await
        });
        assert!(matches!(refused, Err(JsonReadError::Body(BodyReadError::OverCap { cap: 12 }))));
    }
}
