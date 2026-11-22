//! Bearer-token credentials as both server heads hold them: SHA-256 digests only, never the
//! tokens themselves.
//!
//! A head's credential configuration is readable by whoever may read that configuration: a
//! Lambda's environment, a server's tokens file and command line. So neither head keeps a
//! plaintext token to compare against. It hashes the presented bearer and compares digests.
//! Tokens are high-entropy, so an unsalted digest gives nothing away, and a deployer computes
//! one with any common tool (`shasum -a 256`, `openssl dgst -sha256`, terraform's `sha256()`).
//!
//! Each head reads its own sources (environment variables, a TOML file, a flag) and builds one
//! [`Credentials`] from them, so the parsing refusals are the same on both.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::util::object_utils::is_object_hash;
use crate::util::sign_utils;

/// The SHA-256 digest of a bearer token.
///
/// Neither constructor yields the digest of the empty string. A configuration holding it would
/// admit a request with a blank bearer, were a blank bearer ever hashed;
/// [`Credentials::match_bearer`] refuses one before hashing as well.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenDigest([u8; 32]);

impl TokenDigest {
    /// Parse a configured digest: exactly 64 lowercase hex characters, never the digest of the
    /// empty string. The message never echoes the value, which may be a plaintext token put
    /// where its digest belongs.
    pub fn parse_hex(hex: &str) -> Result<TokenDigest, String> {
        // An object hash's spelling is exactly a SHA-256 digest's: 64 lowercase hex characters.
        let malformed = || "a token digest must be 64 lowercase hex characters (a SHA-256 digest)".to_string();

        if !is_object_hash(hex) {
            return Err(malformed());
        }

        let bytes = sign_utils::from_hex(hex).map_err(|()| malformed())?;
        let digest = TokenDigest(bytes.try_into().map_err(|_| malformed())?);
        if digest == TokenDigest::hash("") {
            return Err("a token digest must not be the SHA-256 of the empty string".to_string());
        }

        Ok(digest)
    }

    /// The digest of a token, refused for an empty or whitespace-only token: the same rule
    /// [`Credentials::match_bearer`] applies to a presented bearer. A real token is hashed
    /// untrimmed.
    pub fn of_token(token: &str) -> Result<TokenDigest, String> {
        if token.trim().is_empty() {
            return Err("a bearer token must not be empty or whitespace-only".to_string());
        }

        Ok(TokenDigest::hash(token))
    }

    fn hash(token: &str) -> TokenDigest {
        TokenDigest(Sha256::digest(token.as_bytes()).into())
    }
}

impl std::fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenDigest(<redacted>)")
    }
}

/// Whom a presented bearer token matched ([`Credentials::match_bearer`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bearer<'a> {
    /// The shared bearer: the deployment's own administrator credential.
    Static,

    /// An operator token, bound to this office identifier.
    Operator(&'a str),
}

/// The bearer tokens a head accepts: the shared bearer, per-operator tokens, both, or none (a
/// head that authenticates otherwise, or not at all, decides what none means).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    /// The shared bearer's digest.
    static_token: Option<TokenDigest>,

    /// Per-operator token digests, each bound to an office identifier. Identifiers are kept
    /// untrimmed: the office rules match them by exact equality against the roster.
    operators: HashMap<TokenDigest, String>,
}

/// Counts only: a digest or an identifier never reaches a log line.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("static_token", &self.has_static())
            .field("operators", &self.operator_count())
            .finish()
    }
}

impl Credentials {
    /// Assemble the accepted credentials. Refused when an identifier is blank, and when one
    /// digest is bound twice: among the operators, or as both the shared bearer and an operator,
    /// where the shared bearer's match would silently make that operator an administrator.
    pub fn new(static_token: Option<TokenDigest>,
               operators: Vec<(TokenDigest, String)>) -> Result<Credentials, String> {
        let mut bound = HashMap::new();
        for (digest, identifier) in operators {
            if identifier.trim().is_empty() {
                return Err("an operator token is bound to a blank identifier".to_string());
            }

            if static_token == Some(digest) {
                return Err("an operator token's digest is also the shared bearer's".to_string());
            }

            if bound.insert(digest, identifier).is_some() {
                return Err("one operator token digest is bound more than once".to_string());
            }
        }

        Ok(Credentials { static_token, operators: bound })
    }

    /// Whether no token at all is accepted.
    pub fn is_empty(&self) -> bool {
        self.static_token.is_none() && self.operators.is_empty()
    }

    /// Whether a shared bearer is configured.
    pub fn has_static(&self) -> bool {
        self.static_token.is_some()
    }

    /// How many operator tokens are configured.
    pub fn operator_count(&self) -> usize {
        self.operators.len()
    }

    /// Match a presented bearer token. A blank one (empty or whitespace-only) matches nothing
    /// and is refused before it is hashed. Otherwise its digest is compared with the shared
    /// bearer's in constant time first, then looked up among the operator tokens; the lookup
    /// leaks timing only about the digest.
    pub fn match_bearer(&self, presented: &str) -> Option<Bearer<'_>> {
        if presented.trim().is_empty() {
            return None;
        }

        let digest = TokenDigest::hash(presented);

        if let Some(expected) = &self.static_token {
            if bool::from(digest.0.ct_eq(&expected.0)) {
                return Some(Bearer::Static);
            }
        }

        self.operators.get(&digest).map(|identifier| Bearer::Operator(identifier.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token's digest, spelled as a deployer computes it: independently of [`TokenDigest`].
    fn hex_digest(token: &str) -> String {
        sign_utils::to_hex(&Sha256::digest(token.as_bytes()))
    }

    /// A configured digest is 64 lowercase hex characters, and never the digest of the empty
    /// string: a configuration holding that would admit a blank bearer.
    #[test]
    fn a_token_digest_is_64_lowercase_hex_and_never_the_empty_strings() {
        assert_eq!(TokenDigest::parse_hex(&hex_digest("secret")).unwrap(), TokenDigest::of_token("secret").unwrap());

        assert!(TokenDigest::parse_hex(&hex_digest("")).is_err(), "the empty string's digest");
        assert!(TokenDigest::parse_hex(&hex_digest("secret").to_uppercase()).is_err(), "uppercase");
        assert!(TokenDigest::parse_hex(&hex_digest("secret")[1..]).is_err(), "63 characters");
        assert!(TokenDigest::parse_hex(&format!("{}0", hex_digest("secret"))).is_err(), "65 characters");
        assert!(TokenDigest::parse_hex(&format!("g{}", &hex_digest("secret")[1..])).is_err(), "a non-hex digit");
        assert!(TokenDigest::parse_hex("").is_err(), "empty");
        assert!(TokenDigest::parse_hex("secret").is_err(), "a plaintext token");

        assert!(TokenDigest::of_token("").is_err(), "an empty token");
        assert!(TokenDigest::of_token("   ").is_err(), "a whitespace-only token");
        assert_eq!(
            TokenDigest::of_token(" has spaces ").unwrap(),
            TokenDigest::parse_hex(&hex_digest(" has spaces ")).unwrap(),
            "a real token is hashed untrimmed"
        );

        let debug = format!("{:?}", TokenDigest::of_token("s3cr3t-value").unwrap());
        assert_eq!(debug, "TokenDigest(<redacted>)");
    }

    /// The refusals: a blank identifier, a digest bound twice among the operators, and one
    /// bound as both the shared bearer and an operator. An identifier is kept untrimmed.
    #[test]
    fn credentials_refuse_a_blank_identifier_and_every_double_binding() {
        let digest = |token: &str| TokenDigest::of_token(token).unwrap();

        let error = Credentials::new(None, vec![(digest("ada-token"), "  ".to_string())]).unwrap_err();
        assert_eq!(error, "an operator token is bound to a blank identifier");

        let error = Credentials::new(
            None,
            vec![(digest("ada-token"), "ada@example".to_string()), (digest("ada-token"), "eve@example".to_string())],
        )
        .unwrap_err();
        assert_eq!(error, "one operator token digest is bound more than once");

        let error = Credentials::new(Some(digest("shared")), vec![(digest("shared"), "ada@example".to_string())])
            .unwrap_err();
        assert_eq!(error, "an operator token's digest is also the shared bearer's");

        let kept = Credentials::new(None, vec![(digest("ada-token"), " ada@example".to_string())]).unwrap();
        assert_eq!(kept.match_bearer("ada-token"), Some(Bearer::Operator(" ada@example")));

        assert!(Credentials::new(None, Vec::new()).unwrap().is_empty());
    }

    /// The shared bearer is [`Bearer::Static`], an operator token is that operator, and anything
    /// else (a wrong or near-miss token, an identifier, a token of another case) matches nothing.
    #[test]
    fn a_presented_bearer_matches_its_own_credential_only() {
        let credentials = Credentials::new(
            Some(TokenDigest::parse_hex(&hex_digest("secret")).unwrap()),
            vec![(TokenDigest::parse_hex(&hex_digest("ada-token")).unwrap(), "ada@example".to_string())],
        )
        .unwrap();

        assert_eq!(credentials.match_bearer("secret"), Some(Bearer::Static));
        assert_eq!(credentials.match_bearer("ada-token"), Some(Bearer::Operator("ada@example")));
        assert_eq!(credentials.match_bearer("secrft"), None, "an equal-length near miss");
        assert_eq!(credentials.match_bearer("SECRET"), None, "a token is case-sensitive");
        assert_eq!(credentials.match_bearer("ada@example"), None, "an identifier is not a token");
        assert_eq!(credentials.match_bearer(&hex_digest("secret")), None, "a digest is not its token");
    }

    /// A blank bearer is refused before anything is hashed, so no configured digest can match
    /// one: not even the digest of that very blank string, which [`TokenDigest`]'s constructors
    /// refuse but which is built here directly to show the check does not lean on them.
    #[test]
    fn a_blank_bearer_matches_nothing_before_it_is_hashed() {
        for blank in ["", "   "] {
            let credentials = Credentials {
                static_token: Some(TokenDigest::hash(blank)),
                operators: [(TokenDigest::hash(blank), "ada@example".to_string())].into_iter().collect(),
            };

            assert_eq!(credentials.match_bearer(blank), None, "{:?}", blank);
        }
    }
}
