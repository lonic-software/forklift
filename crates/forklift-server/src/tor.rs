//! Publish the served warehouse as a Tor **v3 onion service**, so a peer set can reach it
//! with no fixed IP, no port-forwarding and no NAT configuration — just a shareable `.onion`
//! (the peer-to-peer transport, DESIGN.html §4.7). The client half is `forklift-core`'s
//! Tor-aware `RemoteClient` (it dials a `.onion` remote through the local Tor SOCKS proxy);
//! this is the host half.
//!
//! It speaks the Tor **control protocol** (a line-based text protocol over a local TCP socket)
//! to a `tor` the operator already runs — no embedded Tor, no new dependency, no cryptography
//! here: authentication is by control password, control cookie, or (a cookie-less control port)
//! null auth, and the onion key type is minted by Tor itself. The onion lives exactly as long
//! as this control connection: dropping [`OnionService`] closes it, and Tor tears the service
//! down — so the address is never left published after the server stops.
//!
//! By default the address is **ephemeral** (a fresh `.onion` each run). Pass a key path to make
//! it **persistent**: the private key Tor mints is saved there (owner-only) on first run and
//! re-offered on every later run, so friends keep one stable address.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

/// The default Tor control address — where a stock local `tor` with `ControlPort 9051` listens.
pub const DEFAULT_CONTROL_ADDR: &str = "127.0.0.1:9051";

/// How the onion service authenticates its key: ephemeral (a new address every run) or backed
/// by a persisted key (one stable address across restarts).
enum KeySpec {
    /// `NEW:ED25519-V3` with `DiscardPK` — Tor mints a throwaway key it never returns.
    Ephemeral,

    /// A key Tor already returned (`ED25519-V3:<blob>`), re-offered to reclaim the same address.
    Persisted(String),

    /// `NEW:ED25519-V3` without `DiscardPK` — Tor mints a key and returns it, so it can be saved.
    NewPersistent,
}

/// A published onion service. Holding it keeps the control connection — and therefore the
/// service — alive; dropping it removes the service. The write half is retained solely for
/// that lifetime binding (the read half stays parked in `_reader` for the same reason).
pub struct OnionService {
    /// The onion address *without* scheme or the `.onion` suffix (Tor's `ServiceID`).
    pub service_id: String,

    /// The virtual port the service exposes (clients reach `http://<id>.onion` when it is 80).
    pub virtual_port: u16,

    /// The live control connection. Kept open on purpose: an onion added without the `Detach`
    /// flag exists only while the connection that created it is open, so this is what keeps the
    /// address published, and dropping it is what tears the address down.
    _control: TcpStream,
    _reader: BufReader<TcpStream>,
}

/// How the onion address is keyed.
pub enum OnionKey<'a> {
    /// A fresh throwaway address every run.
    Ephemeral,

    /// A stable address persisted at this path (created, owner-only, on first run).
    Persistent(&'a Path),
}

impl OnionService {
    /// The full onion URL a peer configures as its `remote.url`.
    pub fn url(&self) -> String {
        if self.virtual_port == 80 {
            format!("http://{}.onion", self.service_id)
        } else {
            format!("http://{}.onion:{}", self.service_id, self.virtual_port)
        }
    }
}

/// Publish an onion service that forwards its `virtual_port` to the locally-bound server.
///
/// # Arguments
/// * `control_addr` - The Tor control address (e.g. `127.0.0.1:9051`).
/// * `password`     - The control password, when the control port uses `HashedControlPassword`.
/// * `virtual_port` - The port the onion exposes (80 lets clients omit the port).
/// * `target`       - The locally-bound server address to forward onion traffic to.
/// * `key`          - Ephemeral, or a path to persist the key at for a stable address.
///
/// # Returns
/// * `Ok(OnionService)` - The published service (kept alive as long as the value lives).
/// * `Err(String)`      - If Tor is unreachable, authentication fails, or the service is refused.
pub fn publish(control_addr: &str,
               password: Option<&str>,
               virtual_port: u16,
               target: SocketAddr,
               key: OnionKey) -> Result<OnionService, String> {
    let control = TcpStream::connect(control_addr).map_err(|e| format!(
        "Could not reach the Tor control port at \"{}\": {}. Is `tor` running with a \
        ControlPort? (see docs/guide/p2p-tor.md)",
        control_addr, e
    ))?;

    // A bounded read timeout so a silent or wrong-protocol peer surfaces as an error rather than
    // hanging the server start forever. Every exchange here is a couple of short lines.
    control.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("Error while configuring the Tor control socket: {}", e))?;

    let mut writer = control.try_clone()
        .map_err(|e| format!("Error while preparing the Tor control connection: {}", e))?;
    let mut reader = BufReader::new(control.try_clone()
        .map_err(|e| format!("Error while preparing the Tor control connection: {}", e))?);

    authenticate(&mut writer, &mut reader, control_addr, password)?;

    // Resolve how to key the address: a persistent path that already holds a key re-offers it;
    // an empty/absent path asks Tor to mint a savable key; no path at all is ephemeral.
    let (key_arg, save_to) = match key {
        OnionKey::Ephemeral => (KeySpec::Ephemeral, None),
        OnionKey::Persistent(path) => match read_key_file(path)? {
            Some(blob) => (KeySpec::Persisted(blob), None),
            None => (KeySpec::NewPersistent, Some(path)),
        },
    };

    let add = match &key_arg {
        KeySpec::Ephemeral =>
            format!("ADD_ONION NEW:ED25519-V3 Flags=DiscardPK Port={},{}", virtual_port, target),
        KeySpec::NewPersistent =>
            format!("ADD_ONION NEW:ED25519-V3 Port={},{}", virtual_port, target),
        KeySpec::Persisted(blob) =>
            format!("ADD_ONION {} Port={},{}", blob, virtual_port, target),
    };

    let lines = command(&mut writer, &mut reader, &add)
        .map_err(|e| format!("Tor refused to publish the onion service: {}", e))?;

    let service_id = field(&lines, "ServiceID=")
        .ok_or("Tor accepted the onion service but returned no ServiceID.".to_string())?;

    // Persist a freshly-minted key so the address survives a restart. Written before returning,
    // owner-only, so a subsequent run reclaims exactly this address.
    if let Some(path) = save_to {
        let blob = field(&lines, "PrivateKey=")
            .ok_or("Tor minted a persistent onion key but returned no PrivateKey to save.".to_string())?;
        write_key_file(path, &blob)?;
    }

    Ok(OnionService {
        service_id,
        virtual_port,
        _control: writer,
        _reader: reader,
    })
}

/// Authenticate the control connection. Tries, in order: the control **password** (when given),
/// the control **cookie** (when Tor advertises a cookie file), then **null** auth (a cookie-less
/// control port). Fails with actionable guidance when none applies.
fn authenticate(writer: &mut TcpStream,
                reader: &mut BufReader<TcpStream>,
                control_addr: &str,
                password: Option<&str>) -> Result<(), String> {
    let info = command(writer, reader, "PROTOCOLINFO 1")
        .map_err(|e| format!("Tor did not answer PROTOCOLINFO on \"{}\": {}", control_addr, e))?;

    if let Some(password) = password {
        return command(writer, reader, &format!("AUTHENTICATE \"{}\"", escape(password)))
            .map(|_| ())
            .map_err(|e| format!(
                "Tor rejected the control password: {}. Check --tor-control-password matches \
                the HashedControlPassword in your torrc.", e
            ));
    }

    if let Some(cookie_path) = cookie_file(&info) {
        let cookie = std::fs::read(&cookie_path).map_err(|e| format!(
            "Tor uses cookie authentication but its cookie file \"{}\" is unreadable: {}. Run \
            the server as a user in Tor's group, or set --tor-control-password.",
            cookie_path, e
        ))?;

        return command(writer, reader, &format!("AUTHENTICATE {}", hex(&cookie)))
            .map(|_| ())
            .map_err(|e| format!("Tor rejected cookie authentication: {}", e));
    }

    // A control port with no password and no cookie accepts null auth.
    command(writer, reader, "AUTHENTICATE")
        .map(|_| ())
        .map_err(|e| format!(
            "Tor's control port requires authentication this build cannot perform \
            automatically: {}. Set a control password (HashedControlPassword in torrc + \
            --tor-control-password), or enable CookieAuthentication.", e
        ))
}

/// Send one control command and read its reply, returning the reply lines' text (the part after
/// the `250-`/`250 ` prefix of each line). A non-2xx final status is an error carrying Tor's own
/// message.
fn command(writer: &mut TcpStream,
           reader: &mut BufReader<TcpStream>,
           line: &str) -> Result<Vec<String>, String> {
    writer.write_all(line.as_bytes())
        .and_then(|_| writer.write_all(b"\r\n"))
        .and_then(|_| writer.flush())
        .map_err(|e| format!("Error while writing to the Tor control port: {}", e))?;

    read_reply(reader)
}

/// Read one control-protocol reply. A reply is a run of lines, each `<3-digit code><sep><text>`,
/// where `sep` is `-` (a mid line), `+` (a mid line introducing a data block that runs until a
/// line of just `.`), or a space (the final line). The final status must be 2xx.
fn read_reply(reader: &mut BufReader<TcpStream>) -> Result<Vec<String>, String> {
    let mut texts: Vec<String> = Vec::new();

    loop {
        let mut line = String::new();

        if reader.read_line(&mut line).map_err(|e| format!(
            "Error while reading from the Tor control port: {}", e
        ))? == 0 {
            return Err("The Tor control connection closed unexpectedly.".to_string());
        }

        let line = line.trim_end_matches(['\r', '\n']);

        if line.len() < 4 {
            return Err(format!("Malformed Tor control reply line: {:?}", line));
        }

        let code = &line[..3];
        let separator = line.as_bytes()[3];
        let text = &line[4..];
        texts.push(text.to_string());

        // A data block (`+`) runs until a line containing only a dot; its lines are not part of
        // the keyed reply text, so they are consumed but dropped.
        if separator == b'+' {
            loop {
                let mut data = String::new();

                if reader.read_line(&mut data).map_err(|e| format!(
                    "Error while reading a Tor data reply: {}", e
                ))? == 0 {
                    return Err("The Tor control connection closed mid data block.".to_string());
                }

                if data.trim_end_matches(['\r', '\n']) == "." {
                    break;
                }
            }
        }

        if separator == b' ' {
            return if code.starts_with('2') {
                Ok(texts)
            } else {
                Err(format!("Tor answered {} {}", code, text))
            };
        }
    }
}

/// The `COOKIEFILE="..."` path from a `PROTOCOLINFO` reply, when Tor advertises cookie auth.
fn cookie_file(info: &[String]) -> Option<String> {
    for line in info {
        if let Some(rest) = line.split("COOKIEFILE=").nth(1) {
            // The path is C-quoted; take the content between the first pair of quotes and
            // undo the two escapes Tor emits (`\\` and `\"`).
            let mut chars = rest.chars();
            if chars.next() != Some('"') {
                continue;
            }

            let mut path = String::new();
            let mut escaped = false;

            for c in chars {
                if escaped {
                    path.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    return Some(path);
                } else {
                    path.push(c);
                }
            }
        }
    }

    None
}

/// Find the value of a `Key=` field across reply lines (e.g. `ServiceID=`, `PrivateKey=`).
fn field(lines: &[String], key: &str) -> Option<String> {
    lines.iter()
        .find_map(|line| line.strip_prefix(key))
        .map(|value| value.trim().to_string())
}

/// Read a persisted onion key blob (`ED25519-V3:<blob>`), or `None` when the path is absent or
/// empty (first run). A present-but-unreadable file is an error, not a silent fresh address.
fn read_key_file(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            Ok(if trimmed.is_empty() { None } else { Some(trimmed.to_string()) })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Error while reading the onion key file \"{}\": {}", path.display(), e
        )),
    }
}

/// Persist a freshly-minted onion key blob, owner-readable only — it is the secret that owns the
/// address. The permissions are set on Unix, where the control cookie/key model this mirrors
/// lives; on other platforms the file inherits the default ACL.
fn write_key_file(path: &Path, blob: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!(
                "Error while creating the onion key folder \"{}\": {}", parent.display(), e
            ))?;
        }
    }

    std::fs::write(path, blob).map_err(|e| format!(
        "Error while writing the onion key file \"{}\": {}", path.display(), e
    ))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!(
            "Error while restricting permissions on the onion key file \"{}\": {}", path.display(), e
        ))?;
    }

    Ok(())
}

/// Escape a control-protocol quoted string (backslash and double-quote).
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Lowercase-hex-encode bytes (for the cookie AUTHENTICATE argument).
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_lowercase_bytes() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn escape_quotes_the_control_string_specials() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn cookie_file_is_parsed_and_unescaped() {
        let info = vec![
            "PROTOCOLINFO 1".to_string(),
            r#"AUTH METHODS=COOKIE,SAFECOOKIE COOKIEFILE="/run/tor/control.authcookie""#.to_string(),
            r#"VERSION Tor="0.4.8.10""#.to_string(),
        ];
        assert_eq!(cookie_file(&info).as_deref(), Some("/run/tor/control.authcookie"));

        // A Windows-style path with escaped backslashes and a quote round-trips.
        let escaped = vec![
            r#"AUTH METHODS=COOKIE COOKIEFILE="C:\\Tor\\a\"b\\cookie""#.to_string(),
        ];
        assert_eq!(cookie_file(&escaped).as_deref(), Some(r#"C:\Tor\a"b\cookie"#));
    }

    #[test]
    fn cookie_file_is_absent_when_not_advertised() {
        let info = vec![
            "AUTH METHODS=NULL".to_string(),
            r#"VERSION Tor="0.4.8.10""#.to_string(),
        ];
        assert!(cookie_file(&info).is_none());
    }

    #[test]
    fn field_reads_the_value_after_the_key() {
        let lines = vec![
            "ServiceID=abcdefghij234567".to_string(),
            "PrivateKey=ED25519-V3:deadbeef".to_string(),
        ];
        assert_eq!(field(&lines, "ServiceID=").as_deref(), Some("abcdefghij234567"));
        assert_eq!(field(&lines, "PrivateKey=").as_deref(), Some("ED25519-V3:deadbeef"));
        assert!(field(&lines, "Nope=").is_none());
    }

    #[test]
    fn an_ephemeral_service_url_omits_port_80() {
        let service = OnionService {
            service_id: "abcdefghij234567".to_string(),
            virtual_port: 80,
            // A pair of connected sockets stands in for the control connection so the struct is
            // constructible in a unit test without a live Tor.
            _control: TcpStream::connect(loopback_listener()).unwrap(),
            _reader: BufReader::new(TcpStream::connect(loopback_listener()).unwrap()),
        };
        assert_eq!(service.url(), "http://abcdefghij234567.onion");

        let on_8080 = OnionService { virtual_port: 8080, ..service };
        assert_eq!(on_8080.url(), "http://abcdefghij234567.onion:8080");
    }

    /// A throwaway loopback listener address to connect a placeholder socket to, so the tests can
    /// build an `OnionService` without a real control port.
    fn loopback_listener() -> SocketAddr {
        use std::net::TcpListener;
        // Leaked on purpose: it must outlive the connect() calls in the test; the OS reclaims it
        // at process exit, and this only runs under `cargo test`.
        let listener = Box::leak(Box::new(TcpListener::bind("127.0.0.1:0").unwrap()));
        listener.local_addr().unwrap()
    }

    /// A minimal fake Tor control port: it answers `PROTOCOLINFO` with `auth_advert`, accepts the
    /// first `AUTHENTICATE`, and answers `ADD_ONION` with `service_id` (plus a `PrivateKey` line
    /// when `private_key` is set). Every command line it receives is forwarded on the returned
    /// channel, so a test can assert the exact handshake `publish` produced.
    fn fake_control_server(auth_advert: &str,
                           service_id: &str,
                           private_key: Option<&str>)
                           -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let auth_advert = auth_advert.to_string();
        let service_id = service_id.to_string();
        let private_key = private_key.map(|key| key.to_string());
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;

            let mut next = |writer: &mut TcpStream, reply: &[&str]| {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                tx.send(line.trim_end_matches(['\r', '\n']).to_string()).unwrap();
                for chunk in reply {
                    writer.write_all(chunk.as_bytes()).unwrap();
                    writer.write_all(b"\r\n").unwrap();
                }
                writer.flush().unwrap();
            };

            // PROTOCOLINFO → advertise auth; AUTHENTICATE → accept.
            next(&mut writer, &["250-PROTOCOLINFO 1", &auth_advert, "250 OK"]);
            next(&mut writer, &["250 OK"]);

            // ADD_ONION → the service id, and the key when the address is persistent.
            let mut reply = vec![format!("250-ServiceID={}", service_id)];
            if let Some(key) = &private_key {
                reply.push(format!("250-PrivateKey={}", key));
            }
            reply.push("250 OK".to_string());
            next(&mut writer, &reply.iter().map(|s| s.as_str()).collect::<Vec<_>>());

            // Hold the connection briefly so `publish` finishes reading before the socket closes
            // (a real onion stays up for the connection's whole life; here we only need the read).
            std::thread::sleep(Duration::from_millis(200));
        });

        (addr, rx)
    }

    /// The whole happy path against a null-auth control port: `publish` runs the handshake and
    /// returns the onion's address, and the `ADD_ONION` it sent is the ephemeral, key-discarding
    /// form mapping the virtual port to the given target.
    #[test]
    fn publish_negotiates_an_ephemeral_onion_over_null_auth() {
        let id = "abcdefghij234567abcdefghij234567abcdefghij234567abcdefghij";
        let (addr, rx) = fake_control_server("250-AUTH METHODS=NULL", id, None);

        let target: SocketAddr = "127.0.0.1:9418".parse().unwrap();
        let service = publish(&addr.to_string(), None, 80, target, OnionKey::Ephemeral)
            .expect("publish should succeed against the fake control port");

        assert_eq!(service.service_id, id);
        assert_eq!(service.url(), format!("http://{}.onion", id));

        assert_eq!(rx.recv().unwrap(), "PROTOCOLINFO 1");
        assert_eq!(rx.recv().unwrap(), "AUTHENTICATE", "null auth sends a bare AUTHENTICATE");
        assert_eq!(
            rx.recv().unwrap(),
            "ADD_ONION NEW:ED25519-V3 Flags=DiscardPK Port=80,127.0.0.1:9418"
        );
    }

    /// The cookie-auth + persistent-key path: `publish` reads the advertised cookie file and
    /// authenticates with its hex, requests a *savable* key (no `DiscardPK`), and writes the
    /// returned private key to the key path so a later run reclaims the address.
    #[test]
    fn publish_authenticates_by_cookie_and_persists_the_key() {
        let dir = std::env::temp_dir().join(format!("forklift-tor-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cookie_path = dir.join("control.authcookie");
        let key_path = dir.join("onion.key");
        std::fs::write(&cookie_path, [0xDE, 0xAD, 0xBE, 0xEF]).unwrap();

        let advert = format!(
            // Real Tor C-escapes the path in this quoted field; do the same so Windows
            // backslashes survive the parser (which unescapes them).
            r#"250-AUTH METHODS=COOKIE,SAFECOOKIE COOKIEFILE="{}""#,
            escape(&cookie_path.to_string_lossy())
        );
        let id = "svcaddress234567svcaddress234567svcaddress234567svcaddre";
        let (addr, rx) = fake_control_server(&advert, id, Some("ED25519-V3:PRIVATEKEYBLOB"));

        let target: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let service = publish(&addr.to_string(), None, 80, target, OnionKey::Persistent(&key_path))
            .expect("publish should succeed with cookie auth");

        assert_eq!(service.service_id, id);

        assert_eq!(rx.recv().unwrap(), "PROTOCOLINFO 1");
        assert_eq!(rx.recv().unwrap(), "AUTHENTICATE deadbeef", "cookie bytes sent as hex");
        let add = rx.recv().unwrap();
        assert!(add.starts_with("ADD_ONION NEW:ED25519-V3 Port=80,127.0.0.1:5000"),
            "a persistent address requests a savable key: {add}");
        assert!(!add.contains("DiscardPK"), "a persistent address must not discard the key: {add}");

        assert_eq!(
            std::fs::read_to_string(&key_path).unwrap(),
            "ED25519-V3:PRIVATEKEYBLOB",
            "the minted key is persisted for the next run"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A control port that offers a persisted key re-offers it verbatim (`ADD_ONION <blob>`),
    /// reclaiming the same address rather than minting a new one.
    #[test]
    fn publish_reoffers_an_existing_persisted_key() {
        let dir = std::env::temp_dir().join(format!("forklift-tor-reoffer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("onion.key");
        std::fs::write(&key_path, "ED25519-V3:EXISTINGKEYBLOB\n").unwrap();

        let id = "reofferaddr234567reofferaddr234567reofferaddr234567reoff";
        let (addr, rx) = fake_control_server("250-AUTH METHODS=NULL", id, None);

        let target: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let service = publish(&addr.to_string(), None, 80, target, OnionKey::Persistent(&key_path))
            .expect("publish should succeed re-offering the key");
        assert_eq!(service.service_id, id);

        assert_eq!(rx.recv().unwrap(), "PROTOCOLINFO 1");
        assert_eq!(rx.recv().unwrap(), "AUTHENTICATE");
        assert_eq!(
            rx.recv().unwrap(),
            "ADD_ONION ED25519-V3:EXISTINGKEYBLOB Port=80,127.0.0.1:6000",
            "the stored key is re-offered verbatim"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
