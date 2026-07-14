use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use argon2::Argon2;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use crate::util::file_utils;

/// The environment variable that overrides the private key directory. Its main purpose
/// is to keep tests away from the developer's real keys (like `FORKLIFT_GLOBAL_CONFIG`).
const ENV_KEYS_DIR: &str = "FORKLIFT_KEYS_DIR";

/// The environment variable that supplies a key's passphrase non-interactively. An
/// escape hatch for CI/automation and the test suite — setting it opts *out* of the
/// interactive-only protection (whoever can read the env can then use the key), so it
/// is for machine identities you accept are passphraseless-in-practice, never for the
/// human-vs-agent boundary a passphrase is meant to enforce.
pub const ENV_KEY_PASSPHRASE: &str = "FORKLIFT_KEY_PASSPHRASE";

/// The first line of an encrypted private key file (a passphrase-protected key). A
/// plaintext key file is instead a bare 64-char hex seed (the historical format), so
/// the two are told apart by content.
const KEY_FILE_MAGIC: &str = "forklift-key-v2";

/// Argon2id parameters for new encrypted keys. Recorded in each file so a future change
/// to these defaults can still decrypt keys written with the old ones. Memory-hard
/// enough to make a stolen key file expensive to brute-force, cheap enough to unlock in
/// well under a second on a laptop.
const ARGON2_MEMORY_KIB: u32 = 19_456; // 19 MiB (OWASP-recommended floor)
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;

/// The head-installed provider that asks the operator for a key's passphrase (the CLI
/// prompts on the terminal). Core never touches a terminal itself — it delegates here,
/// so an unattended context (an agent's non-interactive subprocess) with no provider
/// and no [`ENV_KEY_PASSPHRASE`] simply cannot unlock a protected key: it fails closed.
type PassphraseProvider = Box<dyn Fn(&str) -> Result<String, String> + Send + Sync>;
static PASSPHRASE_PROVIDER: OnceLock<PassphraseProvider> = OnceLock::new();

/// Decrypted key seeds cached for this process only (never persisted): unlocking a
/// protected key once serves every signature in the same command, so a `stack` or
/// `rotate` prompts at most once. Bounded by the keys one command touches.
static SEED_CACHE: OnceLock<Mutex<HashMap<String, [u8; 32]>>> = OnceLock::new();

/// Install the passphrase provider (called once by the head at startup). A second call
/// is ignored.
pub fn set_passphrase_provider(provider: PassphraseProvider) {
    let _ = PASSPHRASE_PROVIDER.set(provider);
}

/// The name of the private key directory inside the user's home directory.
const FOLDER_NAME_KEYS: &str = ".forklift-keys";

/// The file suffix of private key files (named `<key-id>.key`).
const FILE_SUFFIX_PRIVATE_KEY: &str = ".key";

/// The key-owner manifest inside the key directory: `"<key-id>" = "<operator-id>"`.
/// Local bookkeeping only — the office is the authority on key↔operator binding; the
/// manifest lets this machine know which of its own keys belong to which identity
/// (profiles), including keys generated before their admission lands anywhere.
const FILE_NAME_KEY_OWNERS: &str = "owners.toml";

/// The file suffix of parcel signature sidecars in the object store
/// (named like the object, plus this suffix).
pub const FILE_SUFFIX_SIGNATURE: &str = ".sig";

/// A parcel signature: the Ed25519 signature over the parcel's hash (as ASCII bytes),
/// together with the id of the key that produced it.
pub struct ParcelSignature {
    /// The id of the signing key: the Blake3 hex hash of the raw public key bytes.
    pub key_id: String,

    /// The Ed25519 signature bytes (64 bytes).
    pub signature: Vec<u8>,
}

/// Get the directory that holds the operator's private keys.
///
/// # Returns
/// * `Ok(PathBuf)`  - The key directory (it may not exist yet).
/// * `Err(String)` - If the home directory could not be determined.
fn get_keys_dir() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var(ENV_KEYS_DIR) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    #[allow(deprecated)] // The home_dir deprecation was reverted; it is correct on modern platforms.
    std::env::home_dir()
        .filter(|home| !home.as_os_str().is_empty())
        .map(|home| home.join(FOLDER_NAME_KEYS))
        .ok_or("Could not determine the home directory for the key folder.".to_string())
}

/// The path of a private key file.
fn get_private_key_path(key_id: &str) -> Result<PathBuf, String> {
    Ok(get_keys_dir()?.join(format!("{}{}", key_id, FILE_SUFFIX_PRIVATE_KEY)))
}

/// The id of a public key: the Blake3 hex hash of its raw bytes.
///
/// # Arguments
/// * `public_key` - The raw public key bytes.
///
/// # Returns
/// * `String` - The key id.
pub fn key_id_for_public_key(public_key: &[u8]) -> String {
    blake3::hash(public_key).to_hex().to_string()
}

/// Generate a new Ed25519 keypair and store the private key locally
/// (in the key directory, readable only by the owner on Unix). The key's owner is
/// recorded in the local key-owner manifest, so the machine can tell its identities'
/// keys apart (including keys generated before their admission lands anywhere).
///
/// # Arguments
/// * `owner` - The operator id the key belongs to.
///
/// # Returns
/// * `Ok((String, String))` - The key id and the public key (hex).
/// * `Err(String)`          - If the key could not be stored.
pub fn generate_keypair(owner: &str) -> Result<(String, String), String> {
    generate_keypair_inner(owner, None)
}

/// Generate a new Ed25519 keypair whose private key is stored **passphrase-protected**
/// (encrypted at rest): reading the key file is not enough to use it — the passphrase
/// is needed to decrypt it, and signing prompts for it. This is the human-vs-agent
/// boundary: a human's protected key cannot be used by a process that does not have the
/// passphrase (an unattended agent), even one running as the same user.
///
/// # Arguments
/// * `owner`      - The operator id the key belongs to.
/// * `passphrase` - The passphrase to protect the key with.
///
/// # Returns
/// * `Ok((String, String))` - The key id and the public key (hex).
/// * `Err(String)`          - If the key could not be encrypted or stored.
pub fn generate_keypair_encrypted(owner: &str, passphrase: &str) -> Result<(String, String), String> {
    generate_keypair_inner(owner, Some(passphrase))
}

/// Generate and store a keypair, encrypting the private key when a passphrase is given.
fn generate_keypair_inner(owner: &str, passphrase: Option<&str>) -> Result<(String, String), String> {
    let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
    let public_key = signing_key.verifying_key();

    let key_id = key_id_for_public_key(public_key.as_bytes());
    let public_hex = to_hex(public_key.as_bytes());

    let keys_dir = get_keys_dir()?;
    file_utils::create_folder_if_not_exists(&keys_dir)?;

    record_key_owner(&key_id, owner)?;

    let content = match passphrase {
        Some(passphrase) => encrypt_seed(signing_key.as_bytes(), passphrase)?,
        // Plaintext keys keep the historical bare-hex form (no format change), so
        // existing keys and passphraseless machine identities are unaffected.
        None => to_hex(signing_key.as_bytes()),
    };

    let path = get_private_key_path(&key_id)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(&path)
        .map_err(|e| format!("Error while creating the private key file \"{}\": {}", path.to_string_lossy(), e))?;

    file.write_all(content.as_bytes())
        .map_err(|e| format!("Error while writing the private key file: {}", e))?;

    Ok((key_id, public_hex))
}

/// Whether the private key for the given id is passphrase-protected (encrypted at
/// rest). `false` when the key is plaintext or not present.
pub fn is_key_encrypted(key_id: &str) -> bool {
    let Ok(path) = get_private_key_path(key_id) else {
        return false;
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => content.trim_start().starts_with(KEY_FILE_MAGIC),
        Err(_) => false,
    }
}

/// Check whether the private key for the given key id is present locally.
pub fn has_private_key(key_id: &str) -> bool {
    get_private_key_path(key_id).map(|path| path.is_file()).unwrap_or(false)
}

/// Record a key's owner in the local key-owner manifest.
fn record_key_owner(key_id: &str, owner: &str) -> Result<(), String> {
    let path = get_keys_dir()?.join(FILE_NAME_KEY_OWNERS);

    let mut document: toml_edit::DocumentMut = match std::fs::read_to_string(&path) {
        Ok(content) => content.parse()
            .map_err(|e| format!("The key-owner manifest is not valid TOML: {}", e))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml_edit::DocumentMut::default(),
        Err(e) => return Err(format!("Error while reading the key-owner manifest: {}", e)),
    };

    document[key_id] = toml_edit::value(owner);

    file_utils::write_file_atomically(&path, document.to_string().as_bytes())
}

/// The key ids this machine holds for the given owner, per the local manifest.
/// (Keys generated before the manifest existed are simply absent — the office
/// remains the authority on key ownership.)
///
/// # Arguments
/// * `owner` - The operator id.
///
/// # Returns
/// * `Ok(Vec<String>)` - The key ids recorded for the owner.
/// * `Err(String)`     - If the manifest could not be read.
pub fn keys_owned_by(owner: &str) -> Result<Vec<String>, String> {
    let path = get_keys_dir()?.join(FILE_NAME_KEY_OWNERS);

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading the key-owner manifest: {}", e)),
    };

    let document: toml_edit::DocumentMut = content.parse()
        .map_err(|e| format!("The key-owner manifest is not valid TOML: {}", e))?;

    Ok(document.iter()
        .filter(|(_, item)| item.as_str() == Some(owner))
        .map(|(key_id, _)| key_id.to_string())
        .collect())
}

/// Load a locally stored private key.
///
/// # Arguments
/// * `key_id` - The id of the key.
///
/// # Returns
/// * `Ok(SigningKey)` - The signing key.
/// * `Err(String)`    - If the key file is missing or invalid.
fn load_signing_key(key_id: &str) -> Result<SigningKey, String> {
    if let Some(seed) = cached_seed(key_id) {
        return Ok(SigningKey::from_bytes(&seed));
    }

    let path = get_private_key_path(key_id)?;

    let content = std::fs::read_to_string(&path).map_err(|_| format!(
        "The private key {} is not present on this machine (looked for \"{}\").",
        key_id,
        path.to_string_lossy()
    ))?;

    let trimmed = content.trim();

    let seed = if trimmed.starts_with(KEY_FILE_MAGIC) {
        // Passphrase-protected: unlock it (the provider prompts, or the env supplies).
        let passphrase = obtain_passphrase(key_id)?;
        decrypt_seed(trimmed, &passphrase)?
    } else {
        // Plaintext (bare hex seed) — the historical, unprotected form.
        let bytes = from_hex(trimmed)
            .map_err(|_| format!("The private key file \"{}\" is not valid hex.", path.to_string_lossy()))?;

        bytes.try_into()
            .map_err(|_| format!("The private key file \"{}\" does not hold a 32-byte key.", path.to_string_lossy()))?
    };

    cache_seed(key_id, seed);

    Ok(SigningKey::from_bytes(&seed))
}

/// The process-local decrypted-seed cache (unlock once, sign many times per command).
fn cached_seed(key_id: &str) -> Option<[u8; 32]> {
    SEED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(key_id).copied())
}

/// Cache a decrypted seed for the rest of this process.
fn cache_seed(key_id: &str, seed: [u8; 32]) {
    if let Ok(mut cache) = SEED_CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        cache.insert(key_id.to_string(), seed);
    }
}

/// Obtain the passphrase for a protected key: the environment escape hatch first
/// (automation/tests), then the head-installed provider (an interactive prompt). With
/// neither — an unattended agent — this fails, so a protected key cannot be used
/// non-interactively.
fn obtain_passphrase(key_id: &str) -> Result<String, String> {
    if let Ok(passphrase) = std::env::var(ENV_KEY_PASSPHRASE) {
        if !passphrase.is_empty() {
            return Ok(passphrase);
        }
    }

    match PASSPHRASE_PROVIDER.get() {
        Some(provider) => provider(key_id),
        None => Err(format!(
            "Key {} is passphrase-protected, but there is no way to ask for its \
            passphrase here (no interactive terminal, and {} is not set). Run the \
            command interactively, or set {} for automation.",
            key_id, ENV_KEY_PASSPHRASE, ENV_KEY_PASSPHRASE
        )),
    }
}

/// Derive a 32-byte symmetric key from a passphrase with Argon2id (memory-hard, so a
/// stolen key file resists offline brute force).
fn derive_key(passphrase: &str,
              salt: &[u8],
              memory: u32,
              iterations: u32,
              parallelism: u32) -> Result<[u8; 32], String> {
    let params = argon2::Params::new(memory, iterations, parallelism, Some(32))
        .map_err(|e| format!("Invalid Argon2 parameters: {}", e))?;

    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);

    let mut key = [0u8; 32];
    argon2.hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| format!("Error while deriving the key from the passphrase: {}", e))?;

    Ok(key)
}

/// Encrypt a 32-byte key seed under a passphrase, returning the key file contents
/// (Argon2id → ChaCha20-Poly1305; a fresh random salt and nonce per key).
fn encrypt_seed(seed: &[u8; 32], passphrase: &str) -> Result<String, String> {
    use rand::RngCore;

    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    rand::rngs::OsRng.fill_bytes(&mut nonce);

    let key = derive_key(passphrase, &salt, ARGON2_MEMORY_KIB, ARGON2_ITERATIONS, ARGON2_PARALLELISM)?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher.encrypt(Nonce::from_slice(&nonce), seed.as_slice())
        .map_err(|_| "Error while encrypting the private key.".to_string())?;

    Ok(format!(
        "{}\nkdf = argon2id\nmemory = {}\niterations = {}\nparallelism = {}\n\
        salt = {}\nnonce = {}\nciphertext = {}\n",
        KEY_FILE_MAGIC, ARGON2_MEMORY_KIB, ARGON2_ITERATIONS, ARGON2_PARALLELISM,
        to_hex(&salt), to_hex(&nonce), to_hex(&ciphertext)
    ))
}

/// Decrypt an encrypted key file's contents with a passphrase, recovering the seed.
/// A wrong passphrase fails the AEAD tag check and is reported as such.
fn decrypt_seed(content: &str, passphrase: &str) -> Result<[u8; 32], String> {
    let corrupt = || "The encrypted key file is malformed.".to_string();

    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut lines = content.lines();

    if lines.next().map(str::trim) != Some(KEY_FILE_MAGIC) {
        return Err(corrupt());
    }

    for line in lines {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim(), value.trim());
        }
    }

    let field = |name: &str| fields.get(name).copied().ok_or_else(corrupt);
    let number = |name: &str| field(name).and_then(|v| v.parse::<u32>().map_err(|_| corrupt()));
    let bytes = |name: &str| field(name).and_then(|v| from_hex(v).map_err(|_| corrupt()));

    let key = derive_key(
        passphrase,
        &bytes("salt")?,
        number("memory")?,
        number("iterations")?,
        number("parallelism")?,
    )?;

    let ciphertext = bytes("ciphertext")?;
    let nonce = bytes("nonce")?;

    if nonce.len() != 12 {
        return Err(corrupt());
    }

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));

    let seed = cipher.decrypt(Nonce::from_slice(&nonce), ciphertext.as_slice())
        .map_err(|_| "The passphrase is incorrect (or the key file is corrupt).".to_string())?;

    seed.try_into().map_err(|_| corrupt())
}

/// Sign a parcel hash with a locally stored private key. The signature covers the hash's
/// ASCII bytes — and through the hash, transitively, the full parcel: its tree (all
/// content), its parents (all history) and every metadata byte.
///
/// # Arguments
/// * `key_id` - The id of the signing key (its private half must be present locally).
/// * `hash`   - The parcel hash to sign.
///
/// # Returns
/// * `Ok(ParcelSignature)` - The signature.
/// * `Err(String)`         - If the private key is missing or invalid.
pub fn sign_parcel_hash(key_id: &str, hash: &str) -> Result<ParcelSignature, String> {
    Ok(ParcelSignature {
        key_id: key_id.to_string(),
        signature: sign_message(key_id, hash.as_bytes())?,
    })
}

/// Sign an arbitrary message with a locally stored private key. Used for the sigchain
/// records of the office (key endorsements and proofs of possession); parcel signing
/// goes through `sign_parcel_hash`.
///
/// # Arguments
/// * `key_id`  - The id of the signing key (its private half must be present locally).
/// * `message` - The bytes to sign.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The Ed25519 signature bytes (64 bytes).
/// * `Err(String)` - If the private key is missing or invalid.
pub fn sign_message(key_id: &str, message: &[u8]) -> Result<Vec<u8>, String> {
    let signing_key = load_signing_key(key_id)?;

    Ok(signing_key.sign(message).to_bytes().to_vec())
}

/// Verify a signature over an arbitrary message against a public key.
///
/// # Arguments
/// * `public_key_hex` - The public key (hex).
/// * `message`        - The bytes the signature must cover.
/// * `signature`      - The signature bytes.
///
/// # Returns
/// * `Ok(true)`    - If the signature is valid.
/// * `Ok(false)`   - If it is not.
/// * `Err(String)` - If the public key or signature bytes are malformed.
pub fn verify_message(public_key_hex: &str,
                      message: &[u8],
                      signature: &[u8]) -> Result<bool, String> {
    let key_bytes: [u8; 32] = from_hex(public_key_hex)
        .map_err(|_| "The public key is not valid hex.".to_string())?
        .try_into()
        .map_err(|_| "The public key is not 32 bytes.".to_string())?;

    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| "The public key is not a valid Ed25519 key.".to_string())?;

    let signature_bytes: [u8; 64] = signature.to_vec().try_into()
        .map_err(|_| "The signature is not 64 bytes.".to_string())?;

    Ok(verifying_key.verify(message, &Signature::from_bytes(&signature_bytes)).is_ok())
}

/// Verify a parcel signature against a public key.
///
/// # Arguments
/// * `public_key_hex` - The public key (hex).
/// * `hash`           - The parcel hash the signature must cover.
/// * `signature`      - The signature bytes.
///
/// # Returns
/// * `Ok(true)`    - If the signature is valid.
/// * `Ok(false)`   - If it is not.
/// * `Err(String)` - If the public key or signature bytes are malformed.
pub fn verify_parcel_signature(public_key_hex: &str,
                               hash: &str,
                               signature: &[u8]) -> Result<bool, String> {
    verify_message(public_key_hex, hash.as_bytes(), signature)
}

/// The path of a parcel's signature sidecar in the object store.
fn get_signature_path(parcel_hash: &str) -> Result<PathBuf, String> {
    let (folder, filename) = file_utils::get_path_for_object(parcel_hash)?;

    Ok(PathBuf::from(folder).join(format!("{}{}", filename, FILE_SUFFIX_SIGNATURE)))
}

/// Write a parcel's signature sidecar (atomically). The signature lives *next to* the
/// parcel object, never inside it: the parcel hash covers the full object (§4.4), so the
/// signature over that hash cannot be part of the hashed content.
///
/// # Arguments
/// * `parcel_hash` - The hash of the signed parcel.
/// * `signature`   - The signature to store.
///
/// # Returns
/// * `Ok(())`      - If the sidecar was written.
/// * `Err(String)` - If it could not be written.
pub fn store_parcel_signature(parcel_hash: &str,
                              signature: &ParcelSignature) -> Result<(), String> {
    let mut content: Vec<u8> = Vec::new();

    content.extend(crate::util::byte_utils::number_to_vlq_bytes(1)); // format version
    content.extend(crate::util::byte_utils::number_to_vlq_bytes(signature.key_id.len() as u64));
    content.extend(signature.key_id.as_bytes());
    content.extend(crate::util::byte_utils::number_to_vlq_bytes(signature.signature.len() as u64));
    content.extend(&signature.signature);

    let path = get_signature_path(parcel_hash)?;

    file_utils::write_file_atomically(&path, &content)
}

/// Read a parcel's signature sidecar.
///
/// # Arguments
/// * `parcel_hash` - The hash of the parcel.
///
/// # Returns
/// * `Ok(Some(ParcelSignature))` - The stored signature.
/// * `Ok(None)`                  - If the parcel has no signature sidecar.
/// * `Err(String)`               - If the sidecar exists but is malformed.
pub fn load_parcel_signature(parcel_hash: &str) -> Result<Option<ParcelSignature>, String> {
    match load_raw_parcel_signature(parcel_hash)? {
        Some(bytes) => parse_parcel_signature(&bytes, parcel_hash).map(Some),
        None => Ok(None),
    }
}

/// Read the raw bytes of a parcel's signature sidecar (the transfer form — see
/// `docs/format/PARCEL_SIGNATURE_FORMAT.md`).
///
/// # Arguments
/// * `parcel_hash` - The hash of the parcel.
///
/// # Returns
/// * `Ok(Some(Vec<u8>))` - The sidecar bytes.
/// * `Ok(None)`          - If the parcel has no signature sidecar.
/// * `Err(String)`       - If the sidecar could not be read.
pub fn load_raw_parcel_signature(parcel_hash: &str) -> Result<Option<Vec<u8>>, String> {
    let path = get_signature_path(parcel_hash)?;

    match std::fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Error while reading the signature of parcel {}: {}", parcel_hash, e
        )),
    }
}

/// Store raw signature sidecar bytes received from elsewhere (a remote or a bundle).
/// The bytes are validated structurally before the write. A signature is immutable:
/// re-storing identical bytes is a no-op, a conflicting sidecar is refused.
///
/// # Arguments
/// * `parcel_hash` - The hash of the signed parcel.
/// * `bytes`       - The sidecar bytes.
///
/// # Returns
/// * `Ok(())`      - If the sidecar was stored (or was already present, identical).
/// * `Err(String)` - If the bytes are malformed, or a different sidecar already exists.
pub fn store_raw_parcel_signature(parcel_hash: &str, bytes: &[u8]) -> Result<(), String> {
    parse_parcel_signature(bytes, parcel_hash)?;

    if let Some(existing) = load_raw_parcel_signature(parcel_hash)? {
        if existing == bytes {
            return Ok(());
        }

        return Err(format!(
            "Parcel {} already carries a different signature; signatures are immutable.",
            parcel_hash
        ));
    }

    let path = get_signature_path(parcel_hash)?;
    if let Some(parent) = path.parent() {
        // A native bundle may install the signed parcel into a pack, so its loose-object fan-out
        // directory need not exist. The sidecar remains loose and must create that directory.
        file_utils::create_folder_if_not_exists(parent)?;
    }

    file_utils::write_file_atomically(&path, bytes)
}

/// Validate signature sidecar bytes structurally without storing them — the check a
/// server head runs on `PUT /v1/signatures/{hash}` before the office state is known, so a
/// malformed sidecar is rejected early (`422`); the *cryptographic* verification happens
/// later, at ref-update time, when the signing key is known. The AWS head needs this
/// because it stores sidecars in object storage, not on the filesystem
/// `store_raw_parcel_signature` writes to.
///
/// # Arguments
/// * `bytes`       - The sidecar bytes.
/// * `parcel_hash` - The hash of the parcel (for error messages).
///
/// # Returns
/// * `Ok(())`      - If the bytes are a well-formed signature sidecar.
/// * `Err(String)` - If they are malformed.
pub fn validate_raw_parcel_signature(bytes: &[u8], parcel_hash: &str) -> Result<(), String> {
    parse_parcel_signature(bytes, parcel_hash).map(|_| ())
}

/// Parse signature sidecar bytes (see `store_parcel_signature` for the layout).
///
/// # Arguments
/// * `bytes`       - The sidecar bytes.
/// * `parcel_hash` - The hash of the parcel (for error messages).
///
/// # Returns
/// * `Ok(ParcelSignature)` - The parsed signature.
/// * `Err(String)`         - If the bytes are malformed.
fn parse_parcel_signature(bytes: &[u8], parcel_hash: &str) -> Result<ParcelSignature, String> {
    let malformed = || format!("The signature sidecar of parcel {} is malformed.", parcel_hash);

    let mut cursor = 0usize;

    let (version, read) = crate::util::byte_utils::number_from_vlq_bytes(cursor, &bytes)
        .map_err(|_| malformed())?;
    cursor += read;

    if version != 1 {
        return Err(format!(
            "The signature of parcel {} uses unknown format version {}.", parcel_hash, version
        ));
    }

    let (key_id_length, read) = crate::util::byte_utils::number_from_vlq_bytes(cursor, &bytes)
        .map_err(|_| malformed())?;
    cursor += read;

    let key_id_end = cursor + key_id_length as usize;

    if key_id_end > bytes.len() {
        return Err(malformed());
    }

    let key_id = String::from_utf8(bytes[cursor..key_id_end].to_vec()).map_err(|_| malformed())?;
    cursor = key_id_end;

    let (signature_length, read) = crate::util::byte_utils::number_from_vlq_bytes(cursor, &bytes)
        .map_err(|_| malformed())?;
    cursor += read;

    let signature_end = cursor + signature_length as usize;

    if signature_end != bytes.len() {
        return Err(malformed());
    }

    let signature = bytes[cursor..signature_end].to_vec();

    Ok(ParcelSignature { key_id, signature })
}

/// Encode bytes as lowercase hex.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

/// Decode a lowercase/uppercase hex string into bytes.
pub fn from_hex(hex: &str) -> Result<Vec<u8>, ()> {
    if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(());
    }

    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = vec![0u8, 1, 15, 16, 255];

        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert!(from_hex("0g").is_err());
        assert!(from_hex("abc").is_err());
    }

    #[test]
    fn signatures_verify_and_reject_tampering() {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let public_hex = to_hex(signing_key.verifying_key().as_bytes());

        let signature = signing_key.sign(b"parcel-hash").to_bytes().to_vec();

        assert!(verify_parcel_signature(&public_hex, "parcel-hash", &signature).unwrap());
        assert!(!verify_parcel_signature(&public_hex, "other-hash", &signature).unwrap());
    }

    #[test]
    fn a_key_seed_round_trips_through_passphrase_encryption() {
        let seed = [42u8; 32];

        let file = encrypt_seed(&seed, "correct horse battery staple").unwrap();

        // It is a recognizable encrypted file, and the raw seed is nowhere in it.
        assert!(file.starts_with(KEY_FILE_MAGIC));
        assert!(!file.contains(&to_hex(&seed)));

        // The right passphrase recovers the exact seed.
        assert_eq!(decrypt_seed(&file, "correct horse battery staple").unwrap(), seed);

        // A wrong passphrase fails the AEAD tag check — no silent garbage.
        let wrong = decrypt_seed(&file, "wrong passphrase");
        assert!(wrong.is_err());
        assert!(wrong.unwrap_err().contains("passphrase is incorrect"));
    }

    #[test]
    fn each_encryption_uses_a_fresh_salt_and_nonce() {
        let seed = [9u8; 32];

        // Same seed + same passphrase must not produce identical files (random salt/nonce).
        let a = encrypt_seed(&seed, "pw").unwrap();
        let b = encrypt_seed(&seed, "pw").unwrap();
        assert_ne!(a, b);

        // Both still decrypt to the same seed.
        assert_eq!(decrypt_seed(&a, "pw").unwrap(), seed);
        assert_eq!(decrypt_seed(&b, "pw").unwrap(), seed);
    }
}
