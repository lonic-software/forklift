//! A damaged configuration file is refused as `config_damaged` (exit 65) by every command that
//! reads it. In the warehouse's own file `heal` rewrites it when the damage provably cost no
//! setting, and otherwise refuses and names what would go; `heal --config` then keeps only the
//! entries the damage provably spared, after setting the damaged bytes aside. A file that is not
//! damaged but does not read, or that holds a mistake made by hand, is never written over, and
//! does not stop the rest of heal. These
//! tests damage the file the ways Proof Load does (cut short, zeroed, a byte flipped).

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;

const CONFIG_DAMAGED_EXIT: i32 = 65;

const TOKEN: &str = "s3cr3t-token-value";

struct Area {
    base: PathBuf,
    home: PathBuf,
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&self.base);
    }
}

impl Area {
    /// A prepared warehouse with an operator, a mirror setting and a remote with a token.
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-config-heal-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("wh")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let area = Area { home: base.join("home"), base };

        area.run_ok(&["prepare"]);
        area.run_ok(&["config", "set", "operator.identifier", "config-heal@forklift"]);
        area.run_ok(&["config", "set", "mirror.pallets", "main"]);
        area.run_ok(&["config", "set", "remote.url", "https://remote.example"]);
        area.run_ok(&["config", "set", "remote.token", TOKEN]);
        area
    }

    fn root(&self) -> PathBuf {
        self.base.join("wh")
    }

    fn config(&self) -> PathBuf {
        self.root().join(".forklift/config/warehouse.toml")
    }

    fn aside(&self) -> PathBuf {
        self.root().join(".forklift/config/warehouse.toml.damaged")
    }

    fn global(&self) -> PathBuf {
        self.home.join("global-config.toml")
    }

    fn run(&self, args: &[&str]) -> Output {
        forklift_command().args(args)
            .current_dir(self.root())
            .env("FORKLIFT_GLOBAL_CONFIG", self.global())
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output().unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{:?} failed: {}{}", args, text(&output.stdout), text(&output.stderr));
        output
    }

    fn get(&self, key: &str) -> String {
        text(&self.run_ok(&["config", "get", key]).stdout).trim_end().to_string()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parsed(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({}): {}{}", e, text(&output.stdout), text(&output.stderr)))
}

/// The `--json` refusal of `output`, asserting it is `config_damaged` with exit 65.
fn config_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(CONFIG_DAMAGED_EXIT), "expected config_damaged: {}{}",
        text(&output.stdout), text(&output.stderr));
    let value = parsed(output);
    assert_eq!(value["error"]["code"], "config_damaged", "{}", value);
    value
}

/// Flip every bit of the first byte of `needle` in `path`.
fn flip(path: &Path, needle: &str) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = bytes.windows(needle.len()).position(|w| w == needle.as_bytes()).unwrap();
    bytes[at] = !bytes[at];
    std::fs::write(path, bytes).unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Truncate,
    Zero,
    FlipByte,
}

fn damage(path: &Path, kind: Damage) {
    let bytes = std::fs::read(path).unwrap();
    match kind {
        // Cut inside the token's value, so what is left does not parse. A cut whose remainder still
        // parses reads as sound: a file saved by hand without a final newline looks the same.
        Damage::Truncate => {
            let at = bytes.windows(TOKEN.len()).position(|w| w == TOKEN.as_bytes()).unwrap() + 4;
            std::fs::write(path, &bytes[..at]).unwrap()
        }
        Damage::Zero => std::fs::write(path, vec![0u8; bytes.len()]).unwrap(),
        Damage::FlipByte => flip(path, TOKEN),
    }
}

/// Revert: make load_document skip the damage checks (parse the lossy text as before) → each
/// damage reads as not_valid (exit 1) or as sound → red.
#[test]
fn every_damage_is_refused_and_heal_config_mends_it() {
    for kind in [Damage::Truncate, Damage::Zero, Damage::FlipByte] {
        let area = Area::new(&format!("{:?}", kind).to_lowercase());
        damage(&area.config(), kind);
        let damaged = std::fs::read(area.config()).unwrap();

        // Plain heal first, which names what mending would drop before anything is dropped.
        let refused = config_damaged(&area.run(&["--json", "config", "get", "mirror.pallets"]));
        assert_eq!(refused["error"]["next_step"], "forklift heal", "{:?}: {}", kind, refused);

        // Plain heal refuses: each damage here may have cost a setting.
        let refused = config_damaged(&area.run(&["--json", "heal"]));
        assert_eq!(refused["error"]["next_step"], "forklift heal --config", "{:?}: {}", kind, refused);
        assert_eq!(std::fs::read(area.config()).unwrap(), damaged, "{:?}: nothing was changed", kind);
        assert!(std::fs::symlink_metadata(area.aside()).is_err(), "{:?}: nothing set aside", kind);

        let healed = area.run_ok(&["heal", "--config"]);
        let said = format!("{}{}", text(&healed.stdout), text(&healed.stderr));
        assert!(!said.contains(TOKEN), "{:?}: a value is never printed: {}", kind, said);
        assert!(said.contains("rewritten with only the entries the damage provably spared"), "{:?}: {}", kind, said);
        assert_eq!(std::fs::read(area.aside()).unwrap(), damaged, "{:?}: the damaged bytes are set aside", kind);

        // Every command reads the file again, and the dropped remote is gone, url and token alike.
        area.run_ok(&["stocktake"]);
        let url = area.run(&["config", "get", "remote.url"]);
        assert!(!url.status.success(), "{:?}: {}", kind, text(&url.stdout));
        let token = area.run(&["config", "get", "remote.token"]);
        assert!(!token.status.success(), "{:?}: {}", kind, text(&token.stdout));
    }
}

/// Revert: drop the whole-section rule in config_salvage → the url is kept beside a dropped
/// token and pairs with the global one → red.
#[test]
fn a_flipped_token_drops_the_whole_remote_and_keeps_the_rest() {
    let area = Area::new("flip-token");
    flip(&area.config(), TOKEN);

    let healed = parsed(&area.run_ok(&["--json", "heal", "--config"]));
    let dropped: Vec<&str> = healed["data"]["config_repaired"]["dropped"].as_array().unwrap().iter()
        .map(|entry| entry["key"].as_str().unwrap()).collect();
    assert_eq!(dropped, ["remote.url", "remote.token"], "{}", healed);
    assert_eq!(healed["data"]["config_repaired"]["dropped"][0]["falls_back_to_global"], false, "{}", healed);

    assert_eq!(area.get("operator.identifier"), "config-heal@forklift");
    assert_eq!(area.get("mirror.pallets"), "main");
}

/// Revert: make plain heal write only under --config (ignore lossless) → plain heal refuses
/// comment-only damage → red.
#[test]
fn damage_in_a_comment_is_mended_by_plain_heal_with_nothing_set_aside() {
    let area = Area::new("comment");
    flip(&area.config(), "pseudonymous");

    let healed = parsed(&area.run_ok(&["--json", "heal"]));
    assert_eq!(healed["data"]["config_repaired"]["lossless"], true, "{}", healed);
    let healed = area.run_ok(&["heal"]);
    assert!(!text(&healed.stdout).contains("damaged"), "a second heal finds a sound file: {}", text(&healed.stdout));
    assert!(std::fs::symlink_metadata(area.aside()).is_err(), "a lossless repair sets nothing aside");
    // The file's own text is kept, comments too, the damaged character read as U+FFFD.
    // Revert: write the template and the kept entries for a lossless salvage too → red.
    let kept = text(&std::fs::read(area.config()).unwrap());
    assert!(kept.contains("\u{FFFD}seudonymous"), "{}", kept);
    assert_eq!(area.get("remote.token"), TOKEN);
    assert_eq!(area.get("remote.url"), "https://remote.example");
    assert_eq!(area.get("operator.identifier"), "config-heal@forklift");

    // Over a sound file, --config changes nothing.
    let before = std::fs::read(area.config()).unwrap();
    area.run_ok(&["heal", "--config"]);
    assert_eq!(std::fs::read(area.config()).unwrap(), before);
}

/// Revert: overwrite an existing .damaged copy in set_damaged_bytes_aside → the earlier copy is
/// replaced → red.
#[test]
fn an_earlier_damaged_copy_is_never_written_over() {
    let area = Area::new("earlier-copy");
    std::fs::write(area.aside(), b"earlier").unwrap();
    flip(&area.config(), TOKEN);
    let damaged = std::fs::read(area.config()).unwrap();

    let refused = config_damaged(&area.run(&["--json", "heal", "--config"]));
    assert!(refused["error"].get("next_step").is_none(), "{}", refused);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("warehouse.toml.damaged") && message.contains("Move that copy elsewhere"), "{}", refused);
    assert_eq!(std::fs::read(area.aside()).unwrap(), b"earlier");
    assert_eq!(std::fs::read(area.config()).unwrap(), damaged);
}

/// A mistake made by hand anywhere but the last line is no cut, final newline or not. Revert:
/// drop the body-parses condition from is_cut → the unended file reads as damaged → red.
#[test]
fn a_mistake_made_by_hand_keeps_its_by_hand_refusal_and_is_not_written_over() {
    let area = Area::new("hand");
    let sound = std::fs::read(area.config()).unwrap();
    for (ending, unended) in [(&b"bogus = \n"[..], false), (&b"bogus = \n[mirror2]"[..], true)] {
        let mut bytes = sound.clone();
        bytes.extend_from_slice(ending);
        std::fs::write(area.config(), &bytes).unwrap();

        let refused = area.run(&["--json", "config", "get", "mirror.pallets"]);
        assert_eq!(refused.status.code(), Some(1), "unended {}: {}", unended, text(&refused.stdout));
        assert!(parsed(&refused)["error"]["message"].as_str().unwrap().contains("Fix the file by hand"));

        // Heal does not mend it and is not stopped by it: the remote read is a note.
        // Revert: return load_scope's error from repair_warehouse_config → heal exits 1 → red.
        let healed = area.run_ok(&["heal", "--config"]);
        assert!(text(&healed.stdout).contains("Fix the file by hand"), "{}", text(&healed.stdout));
        assert_eq!(std::fs::read(area.config()).unwrap(), bytes);
    }
}

/// A last line no newline ends, which does not parse, may be a cut or a hand edit: the refusal
/// says how to tell, and plain heal names the loss before anything is lost.
#[test]
fn an_unended_last_line_names_both_readings() {
    let area = Area::new("unended");
    let mut bytes = std::fs::read(area.config()).unwrap();
    bytes.extend_from_slice(b"profile = work");
    std::fs::write(area.config(), &bytes).unwrap();

    let refused = config_damaged(&area.run(&["--json", "config", "get", "mirror.pallets"]));
    assert!(refused["error"]["message"].as_str().unwrap().contains("edited by hand rather than cut"), "{}", refused);
    let refused = config_damaged(&area.run(&["--json", "heal"]));
    assert!(refused["error"]["message"].as_str().unwrap().contains("edited by hand rather than cut"), "{}", refused);
    assert_eq!(std::fs::read(area.config()).unwrap(), bytes);
}

/// The set-aside copy can hold the token, so it is never wider than the file it copies, not even
/// for a moment. Revert: write it with replace_file_atomically (the umask's mode) → 0644 → red.
#[cfg(unix)]
#[test]
fn the_damaged_copy_keeps_the_files_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("mode");
    std::fs::set_permissions(area.config(), std::fs::Permissions::from_mode(0o600)).unwrap();
    flip(&area.config(), TOKEN);
    area.run_ok(&["heal", "--config"]);
    assert_eq!(std::fs::metadata(area.aside()).unwrap().permissions().mode() & 0o777, 0o600);
}

/// A file that cannot be read at all may be sound: heal leaves it as it is, and is not stopped.
#[test]
fn a_file_that_does_not_read_is_never_written_over() {
    let area = Area::new("unreadable");
    forklift_core::util::file_utils::remove_file(area.config()).unwrap();
    std::fs::create_dir(area.config()).unwrap();

    let healed = area.run(&["heal", "--config"]);
    assert_ne!(healed.status.code(), Some(CONFIG_DAMAGED_EXIT), "{}", text(&healed.stderr));
    assert!(std::fs::metadata(area.config()).unwrap().is_dir());
}

/// Revert: have heal.rs abort on a failed remote.url read (`?` instead of the note) → a damaged
/// global file stops heal in a warehouse it has no next step for → red.
#[test]
fn a_damaged_global_file_is_refused_without_a_next_step_and_does_not_stop_heal() {
    let area = Area::new("global");
    // Unset here, so heal's read of the remote's address reaches the global file.
    area.run_ok(&["config", "unset", "remote.token"]);
    area.run_ok(&["config", "unset", "remote.url"]);
    std::fs::write(area.global(), b"[operator]\nidentifier = \"g\0\"\n").unwrap();

    let refused = config_damaged(&area.run(&["--json", "config", "get", "--global", "operator.identifier"]));
    assert!(refused["error"].get("next_step").is_none(), "{}", refused);
    assert!(refused["error"]["message"].as_str().unwrap().contains("Fix the file by hand"), "{}", refused);

    let healed = area.run_ok(&["heal"]);
    assert!(text(&healed.stdout).contains("could not be read"), "{}", text(&healed.stdout));
}

/// A cut inside a multi-line string is a cut, not a hand mistake. Revert: try no closer in
/// parse_closed → is_cut fails, the file reads as not_valid (exit 1) and heal leaves it → red.
#[test]
fn a_cut_inside_a_multi_line_token_is_damage_heal_config_mends() {
    let area = Area::new("multi-line-cut");
    let written = text(&std::fs::read(area.config()).unwrap());
    let quoted = format!("\"{}\"", TOKEN);
    assert!(written.contains(&quoted), "{}", written);
    let multi = written.replace(&quoted, &format!("\"\"\"\n{}\n{}\"\"\"", &TOKEN[..6], &TOKEN[6..]));
    let at = multi.find(&TOKEN[6..]).unwrap() + 3;
    std::fs::write(area.config(), &multi.as_bytes()[..at]).unwrap();

    config_damaged(&area.run(&["--json", "config", "get", "mirror.pallets"]));
    area.run_ok(&["heal", "--config"]);
    assert_eq!(area.get("operator.identifier"), "config-heal@forklift");
    assert!(!area.run(&["config", "get", "remote.token"]).status.success());
}

/// A byte flipped to a carriage return is damage, mended by plain heal when it hides nothing.
/// Revert: let content_damage pass a lone carriage return → not_valid (exit 1) → red.
#[test]
fn a_lone_carriage_return_is_damage_plain_heal_mends() {
    let area = Area::new("lone-cr");
    let mut bytes = std::fs::read(area.config()).unwrap();
    let at = bytes.windows(12).position(|w| w == b"pseudonymous").unwrap();
    bytes[at] = b'\r';
    std::fs::write(area.config(), &bytes).unwrap();

    let refused = config_damaged(&area.run(&["--json", "config", "get", "mirror.pallets"]));
    assert!(refused["error"]["message"].as_str().unwrap().contains("U+000D"), "{}", refused);
    let healed = parsed(&area.run_ok(&["--json", "heal"]));
    assert_eq!(healed["data"]["config_repaired"]["lossless"], true, "{}", healed);
    assert_eq!(area.get("remote.token"), TOKEN);
}

/// A salvage that loses no entry but whose text does not validate, here for an unknown section,
/// is mended only by the lossy rewrite. Revert: trust salvage.lossless alone → plain heal tries
/// the lossless write, which fails validation, and heal aborts uncoded → red.
#[test]
fn a_lossless_salvage_that_does_not_validate_is_refused_for_heal_config() {
    let area = Area::new("invalid-lossless");
    let mut bytes = std::fs::read(area.config()).unwrap();
    bytes.extend_from_slice(b"[bogus]\n");
    let at = bytes.windows(12).position(|w| w == b"pseudonymous").unwrap();
    bytes[at] = !bytes[at];
    std::fs::write(area.config(), &bytes).unwrap();

    let refused = config_damaged(&area.run(&["--json", "heal"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal --config", "{}", refused);
    assert!(refused["error"]["message"].as_str().unwrap().contains("does not hold a valid configuration"),
            "{}", refused);
    let healed = parsed(&area.run_ok(&["--json", "heal", "--config"]));
    assert_eq!(healed["data"]["config_repaired"]["lossless"], false, "{}", healed);
    assert_eq!(area.get("remote.token"), TOKEN);
}
