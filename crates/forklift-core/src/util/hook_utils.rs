//! Request signing and verification for the hook protocol
//! (`docs/format/HOOK_PROTOCOL.md`).
//!
//! Every hook request is authenticated by a Blake3 keyed MAC over the timestamp and
//! the body — mutual authentication is not optional (a spoofable authentication hook
//! is game over, §8.13), so a hook cannot be configured without a secret. Only the
//! server head *calls* hooks (it holds the URLs and secrets); the client reaches
//! resolution through the server (`POST /v1/resolve`), never a hook directly.

use crate::model::hooks::{
    HEADER_HOOK, HEADER_HOOK_SIGNATURE, HEADER_HOOK_TIMESTAMP, HEADER_HOOK_VERSION,
    HOOK_PROTOCOL_VERSION,
};

/// The key-derivation context of the request MAC. Versioned with the protocol: a
/// wire-format change re-keys every MAC, so mixed versions can never half-verify.
const MAC_CONTEXT: &str = "forklift hook protocol 2026-11-22 request mac";

/// How far a request's timestamp may lie from the receiver's clock before the
/// request is refused as a replay (seconds, either direction).
pub const MAX_CLOCK_SKEW_SECONDS: i64 = 300;

/// Compute the request MAC: Blake3 keyed hash (hex) over `"<timestamp>\n" + body`,
/// keyed by `derive_key(MAC_CONTEXT, secret)`.
pub fn sign_hook_request(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let key = blake3::derive_key(MAC_CONTEXT, secret.as_bytes());

    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(timestamp.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(body);

    hasher.finalize().to_hex().to_string()
}

/// Verify a request MAC and its freshness — what a hook endpoint runs before acting.
///
/// # Returns
/// * `Err(String)` - If the timestamp is outside the skew window or the MAC does not
///                   match (the two cases are deliberately not distinguished for the
///                   caller beyond the message).
pub fn verify_hook_request(secret: &str,
                           timestamp: i64,
                           now: i64,
                           body: &[u8],
                           signature: &str) -> Result<(), String> {
    if (now - timestamp).abs() > MAX_CLOCK_SKEW_SECONDS {
        return Err(format!(
            "The request timestamp {} is outside the {}s replay window.",
            timestamp, MAX_CLOCK_SKEW_SECONDS
        ));
    }

    let expected = sign_hook_request(secret, timestamp, body);

    // Blake3 outputs are fixed-length hex; a byte-wise comparison of equal-length
    // MACs leaks nothing useful here (the MAC key never travels).
    if expected.as_bytes() != signature.as_bytes() {
        return Err("The request signature does not verify.".to_string());
    }

    Ok(())
}

/// The headers of a signed hook request: `(name, value)` pairs ready to set.
pub fn hook_request_headers(hook: &str,
                            secret: &str,
                            timestamp: i64,
                            body: &[u8]) -> [(&'static str, String); 4] {
    [
        (HEADER_HOOK, hook.to_string()),
        (HEADER_HOOK_VERSION, HOOK_PROTOCOL_VERSION.to_string()),
        (HEADER_HOOK_TIMESTAMP, timestamp.to_string()),
        (HEADER_HOOK_SIGNATURE, sign_hook_request(secret, timestamp, body)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mac_verifies_and_binds_timestamp_body_and_secret() {
        let body = br#"{"token":"t"}"#;
        let mac = sign_hook_request("secret", 1000, body);

        assert!(verify_hook_request("secret", 1000, 1000, body, &mac).is_ok());
        assert!(verify_hook_request("secret", 1000, 1100, body, &mac).is_ok());

        // A different secret, a shifted timestamp or a changed body all fail.
        assert!(verify_hook_request("other", 1000, 1000, body, &mac).is_err());
        assert!(verify_hook_request("secret", 1001, 1001, body, &mac).is_err());
        assert!(verify_hook_request("secret", 1000, 1000, b"{}", &mac).is_err());
    }

    /// The protocol version a request carries, on its header and in its MAC key, read against
    /// literals: a receiver written for another version refuses the request rather than parsing
    /// a vocabulary it does not know.
    #[test]
    fn a_request_speaks_the_2026_11_22_protocol() {
        let body = br#"{"action":"fetch"}"#;
        let headers = hook_request_headers("admission", "secret", 1000, body);

        assert_eq!(headers[1], (HEADER_HOOK_VERSION, "2026-11-22".to_string()));

        let key = blake3::derive_key("forklift hook protocol 2026-11-22 request mac", b"secret");
        let mut hasher = blake3::Hasher::new_keyed(&key);
        hasher.update(b"1000\n");
        hasher.update(body);
        let expected = hasher.finalize().to_hex().to_string();

        assert_eq!(headers[3], (HEADER_HOOK_SIGNATURE, expected.clone()));
        assert!(verify_hook_request("secret", 1000, 1000, body, &expected).is_ok());
    }

    #[test]
    fn stale_requests_are_refused_in_both_directions() {
        let body = b"x";
        let mac = sign_hook_request("secret", 1000, body);

        let stale = verify_hook_request("secret", 1000, 1000 + MAX_CLOCK_SKEW_SECONDS + 1, body, &mac);
        assert!(stale.is_err());
        assert!(stale.unwrap_err().contains("replay window"));

        let future = verify_hook_request("secret", 1000, 1000 - MAX_CLOCK_SKEW_SECONDS - 1, body, &mac);
        assert!(future.is_err());
    }
}
