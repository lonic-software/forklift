//! `docs/guide/recovery.md` names a way out of each refusal. These tests make the refused state with
//! the real binaries, and run the way out the page names, so a remedy cannot go in on faith or stop
//! working unnoticed. A refusal another suite already drives to its way out is not repeated here.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::common;
use common::{forklift_command, repository_root, server_binary, ServerProcess};

struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let root = std::env::temp_dir().join(format!("forklift-recovery-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Area { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn run(&self, dir: &str, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(self.path(dir))
            .env("FORKLIFT_GLOBAL_CONFIG", self.path("global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.path("keys"))
            .output()
            .unwrap()
    }

    #[track_caller]
    fn ok(&self, dir: &str, args: &[&str]) -> String {
        let output = self.run(dir, args);
        assert!(output.status.success(), "{:?}: {}{}", args, stdout(&output), stderr(&output));
        stdout(&output)
    }

    /// Run `args`, which must refuse with `code`, and return the error document.
    #[track_caller]
    fn refused(&self, dir: &str, args: &[&str], code: &str, exit: i32) -> serde_json::Value {
        let output = self.run(dir, &[&["--json"][..], args].concat());
        assert_eq!(output.status.code(), Some(exit), "{:?}: {}{}", args, stdout(&output), stderr(&output));
        let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(value["error"]["code"], code, "{}", value);
        value["error"].clone()
    }

    fn write(&self, relative: &str, content: &str) {
        std::fs::write(self.path(relative), content).unwrap();
    }

    fn prepare(&self, dir: &str) {
        std::fs::create_dir_all(self.path(dir)).unwrap();
        self.ok(dir, &["prepare"]);
        self.ok(dir, &["config", "set", "--global", "operator.identifier", "tester@recovery"]);
    }

    fn stack(&self, dir: &str, file: &str, content: &str) {
        self.write(&format!("{}/{}", dir, file), content);
        self.ok(dir, &["load", file]);
        self.ok(dir, &["stack", "-m", content]);
    }

    /// The one pack `store compact --all` left, without its extension.
    fn only_pack(&self, dir: &str) -> PathBuf {
        let folder = self.path(dir).join(".forklift/objects/pack");
        let packs: Vec<PathBuf> = std::fs::read_dir(&folder).unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "{:?}", packs);
        packs[0].with_extension("")
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&self.root);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn object_files(folder: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(object_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

struct Server {
    process: ServerProcess,
    url: String,
}

impl Server {
    /// Serve a bare warehouse at `area/name` on a port of its own. The static token is what a
    /// remote wants before it accepts a re-genesis.
    fn start(area: &Area, name: &str) -> Server {
        let root = area.path(name);
        let root = root.to_str().unwrap();
        let prepared = Command::new(server_binary()).args(["prepare", "--root", root]).output().unwrap();
        assert!(prepared.status.success(), "{}", stderr(&prepared));
        Server::serve(root)
    }

    /// The SHA-256 of `secret`, the token every client in this file presents
    /// (`printf secret | shasum -a 256`).
    const SECRET_SHA256: &str = "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

    fn serve(root: &str) -> Server {
        let mut process = ServerProcess::new(Command::new(server_binary())
            .args(["serve", "--token-sha256", Self::SECRET_SHA256, "--root", root, "--addr", "127.0.0.1:0"])
            .env("FORKLIFT_FSYNC", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap());
        let mut line = String::new();
        BufReader::new(process.child().stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        Server { process, url }
    }
}

/// `a` lifted one parcel to a new server, and `b` is a clone of it.
fn two_clones(area: &Area) -> Server {
    let server = Server::start(area, "server");
    area.prepare("a");
    area.ok("a", &["config", "set", "remote.url", &server.url]);
    area.ok("a", &["config", "set", "remote.token", "secret"]);
    area.stack("a", "f.txt", "one\n");
    area.ok("a", &["lift"]);
    area.ok(".", &["clone", &server.url, "b", "--token", "secret"]);
    server
}

/// The `(code, exit)` of each row of the generated table.
fn table_codes() -> Vec<(String, String)> {
    let table = std::fs::read_to_string(repository_root().join("docs/generated/errors.md")).unwrap();
    table.lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .map(|row| {
            let mut cells = row.split('|');
            let code = cells.next().unwrap().trim().trim_end_matches('`').to_string();
            let exit = cells.next().unwrap().trim().to_string();
            (code, exit)
        })
        .filter(|(_, exit): &(String, String)| exit.parse::<i32>().is_ok())
        .collect()
}

#[test]
fn the_page_has_an_entry_for_every_code_with_its_exit() {
    let page = std::fs::read_to_string(repository_root().join("docs/guide/recovery.md")).unwrap();
    let headings: Vec<&str> = page.lines().filter(|line| line.starts_with("### ")).collect();
    let table = table_codes();
    assert!(table.len() > 30, "the table was not read: {:?}", table);
    for (code, exit) in &table {
        let entry = format!("`{}` (exit {})", code, exit);
        assert!(headings.iter().any(|heading| heading.contains(&entry)), "no heading holds {}", entry);
    }
    // And no entry names a code or an exit the table does not. Backticks alternate, so every odd
    // piece of a heading is a code and the piece after it holds that code's exit.
    for heading in headings {
        let pieces: Vec<&str> = heading.split('`').collect();
        for pair in pieces[1..].chunks(2) {
            let [code, rest] = pair else { continue };
            let Some(rest) = rest.strip_prefix(" (exit ") else { continue };
            let exit = rest.split(')').next().unwrap();
            assert!(table.iter().any(|(c, e)| c == code && e == exit), "{} (exit {}) is not in the table", code, exit);
        }
    }
}

/// Hold the kernel lock on the sidecar at `path` from this test process, as another forklift
/// process that is still running would.
fn hold_lock(path: &Path) -> std::fs::File {
    let sidecar = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).unwrap();
    sidecar.try_lock().expect("the lock is free before the test holds it");
    sidecar
}

/// `warehouse_locked`, `store_locked` and `serve_locked`: each is refused while another process
/// holds its lock, with a way out that waits (or stops the server) and removes nothing, and the
/// command runs once the holder lets go.
///
/// Revert: name a removal in a refusal → the "remov" assertion is red.
#[test]
fn a_held_lock_is_refused_until_its_holder_lets_go() {
    let area = Area::new("lock");
    area.prepare("w");
    area.write("w/a.txt", "a\n");

    let cases: [(&str, &[&str], &str, &str); 3] = [
        ("w/.forklift/lock", &["load", "a.txt"], "warehouse_locked", "Wait for the other forklift process to finish"),
        ("w/.forklift/store.lock", &["store", "compact"], "store_locked", "Wait for that maintenance to finish"),
        ("w/.forklift/serve.lock", &["store", "compact", "--all"], "serve_locked", "Stop the server serving it"),
    ];
    for (lock, args, code, way_out) in cases {
        let held = hold_lock(&area.path(lock));
        let error = area.refused("w", args, code, 6);
        assert!(error["message"].as_str().unwrap().contains("still running"), "{}", error);
        assert!(error["next_step"].as_str().unwrap().starts_with(way_out), "{}", error);
        assert!(!error.to_string().to_lowercase().contains("remov"), "the way out removes nothing: {}", error);

        drop(held);
        area.ok("w", args);
    }
}

/// A forklift process that dies holding the warehouse lock holds nothing: the next command runs
/// with nothing removed.
///
/// Revert: a lock file the holder must delete → the command after the crash is refused → red.
#[test]
fn a_lock_a_dead_process_held_refuses_nothing() {
    let area = Area::new("dead-lock");
    area.prepare("w");
    area.write("w/a.txt", "a\n");

    // Exits just before its first rename, holding the lock, with no destructor run.
    let mut load = forklift_command();
    let crashed = load.args(["load", "a.txt"]).current_dir(area.path("w"))
        .env("FORKLIFT_GLOBAL_CONFIG", area.path("global.toml")).env("FORKLIFT_KEYS_DIR", area.path("keys"))
        .env("FORKLIFT_DEBUG_CRASH_AT_RENAME", "1").output().unwrap();
    assert_eq!(crashed.status.code(), Some(forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE), "{}", stderr(&crashed));
    assert!(area.path("w/.forklift/lock").exists(), "the dead holder's sidecar stays");

    area.ok("w", &["load", "a.txt"]);
}

/// A server killed with SIGKILL holds nothing: `gc` runs, and the server serves again, with
/// nothing removed — what a supervisor restarting it relies on.
///
/// Revert: a serve lock file a killed server leaves → `gc` is refused → red.
#[test]
fn a_hard_killed_server_holds_no_serve_lock() {
    let area = Area::new("serve-lock");
    let mut server = Server::start(&area, "server");
    server.process.stop();
    let root = area.path("server");
    let root = root.to_str().unwrap();
    assert!(area.path("server/.forklift/serve.lock").exists(), "the killed server's sidecar stays");

    let gc = Command::new(server_binary()).args(["gc", "--root", root]).output().unwrap();
    assert!(gc.status.success(), "{}", stderr(&gc));
    let restarted = Server::serve(root);
    assert!(restarted.url.starts_with("http://"), "the server serves again: {:?}", restarted.url);
}

#[cfg(unix)]
#[test]
fn a_failed_load_is_cleared_by_loading_again_or_by_unloading() {
    use std::os::unix::fs::PermissionsExt;

    for way_out in ["load", "unload"] {
        let area = Area::new(&format!("load-{}", way_out));
        area.prepare("w");
        std::fs::create_dir(area.path("w/d")).unwrap();
        area.write("w/d/blocked.txt", "x\n");
        let blocked = area.path("w/d/blocked.txt");
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // A test run as root can read it anyway, so there is no failed load to recover from.
        if std::fs::read(&blocked).is_ok() {
            return;
        }
        assert!(!area.run("w", &["load", "."]).status.success());
        area.refused("w", &["stack", "-m", "x"], "incomplete_load", 20);

        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
        area.ok("w", &[way_out, "."]);
        if way_out == "unload" {
            area.write("w/kept.txt", "kept\n");
            area.ok("w", &["load", "kept.txt"]);
        }
        area.ok("w", &["stack", "-m", "x"]);
    }
}

#[test]
fn staged_content_lost_from_the_store_is_staged_again_from_the_working_copy() {
    let area = Area::new("staged");
    area.prepare("w");
    area.stack("w", "a.txt", "a\n");
    area.write("w/c.txt", "c\n");
    let before = object_files(&area.path("w/.forklift/objects"));
    area.ok("w", &["load", "c.txt"]);
    let added: Vec<PathBuf> = object_files(&area.path("w/.forklift/objects"))
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    assert!(!added.is_empty(), "the load wrote no object");
    for object in added {
        forklift_core::util::file_utils::remove_file(object).unwrap();
    }

    area.refused("w", &["stack", "-m", "x"], "staged_content_missing", 25);

    area.ok("w", &["unload", "--", "c.txt"]);
    area.ok("w", &["load", "--", "c.txt"]);
    area.ok("w", &["stack", "-m", "x"]);
}

#[test]
fn a_remote_ahead_is_cleared_by_lowering() {
    let area = Area::new("ahead");
    let _server = two_clones(&area);
    area.stack("a", "f.txt", "two\n");
    area.ok("a", &["lift"]);

    let error = area.refused("b", &["lift"], "remote_ahead", 28);
    assert_eq!(error["next_step"], "forklift lower", "{}", error);
    area.ok("b", &["lower"]);
    assert_eq!(std::fs::read_to_string(area.path("b/f.txt")).unwrap(), "two\n");
}

#[test]
fn a_divergence_is_cleared_by_the_commands_the_page_lists() {
    let area = Area::new("diverged");
    let _server = two_clones(&area);
    area.stack("a", "f.txt", "from a\n");
    area.ok("a", &["lift"]);
    area.stack("b", "f.txt", "from b\n");

    let error = area.refused("b", &["lift"], "diverged", 5);
    assert_eq!(error["next_step"], "forklift lower; then forklift lift");

    // The lower merges the remote's head in and stops on the conflict.
    let error = area.refused("b", &["lower"], "merge_conflicts", 47);
    assert_eq!(error["details"]["paths"], serde_json::json!(["f.txt"]));
    area.write("b/f.txt", "both\n");
    area.ok("b", &["load", "f.txt"]);
    area.ok("b", &["stack", "-m", "both"]);
    area.ok("b", &["lift"]);

    area.ok("a", &["lower"]);
    assert_eq!(std::fs::read_to_string(area.path("a/f.txt")).unwrap(), "both\n");
}

#[test]
fn a_quarantined_pack_comes_back_from_its_file_or_a_fresh_clone_and_heal_names_what_is_lost() {
    let area = Area::new("quarantine");
    let server = two_clones(&area);
    area.ok("a", &["store", "compact", "--all"]);
    let pack = area.only_pack("a").with_extension("pack");
    let saved = area.path("saved.pack");
    forklift_core::util::file_utils::rename(&pack, &saved).unwrap();

    let status: serde_json::Value = serde_json::from_str(&area.ok("a", &["--json", "store", "status"])).unwrap();
    assert_eq!(status["data"]["quarantined_packs"].as_array().unwrap().len(), 1, "{}", status);

    let error = area.refused("a", &["stocktake"], "object_unreadable", 57);
    assert_eq!(error["next_step"], "forklift heal", "{}", error);

    // Heal cannot rebuild an index whose data file is gone: it names what is lost and where it is.
    let error = area.refused("a", &["heal"], "objects_lost", 58);
    let lost = &error["details"]["objects_lost"];
    assert!(lost["unreadable"].as_u64().unwrap() > 0, "{}", error);
    assert_eq!(lost["pallets"], serde_json::json!(["main"]), "{}", error);
    assert!(error["next_step"].as_str().unwrap().starts_with("restore the warehouse from a backup"), "{}", error);
    area.ok("a", &["lower"]);
    assert!(!area.run("a", &["stocktake"]).status.success(), "lower would be a way out");

    area.ok(".", &["clone", &server.url, "fresh", "--token", "secret"]);
    area.ok("fresh", &["stocktake"]);

    forklift_core::util::file_utils::rename(&saved, &pack).unwrap();
    area.ok("a", &["stocktake"]);
}

#[test]
fn heal_rebuilds_a_lost_unparseable_or_damaged_index_from_its_data_file() {
    let area = Area::new("packs");
    area.prepare("w");
    area.stack("w", "f.txt", "one\n");
    area.stack("w", "f.txt", "two\n");
    area.ok("w", &["store", "compact", "--all"]);
    let index = area.only_pack("w").with_extension("idx");
    let original = std::fs::read(&index).unwrap();
    let status = |list: &str| -> usize {
        let value: serde_json::Value = serde_json::from_str(&area.ok("w", &["--json", "store", "status"])).unwrap();
        value["data"][list].as_array().unwrap().len()
    };
    let kept_aside = || std::fs::read_dir(index.parent().unwrap()).unwrap()
        .filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().contains(".idx.damaged-"))
        .count();

    let mut unparsable = original.clone();
    unparsable[0] ^= 0xff;
    let mut damaged = original.clone();
    let near_end = damaged.len() - 40;
    damaged[near_end] ^= 0xff;

    for (what, bytes, list) in [("lost", None, None), ("unparsable", Some(&unparsable), Some("unenumerable_indexes")), ("damaged", Some(&damaged), Some("damaged_packs"))] {
        match bytes {
            Some(bytes) => std::fs::write(&index, bytes).unwrap(),
            None => forklift_core::util::file_utils::remove_file(&index).unwrap(),
        }
        if let Some(list) = list {
            assert_eq!(status(list), 1, "{}", what);
        }
        let aside = kept_aside();
        area.refused("w", &["history"], "object_unreadable", 57);
        // A stack reads the head's tree to build on it, so it meets the damage the same way. Only
        // the first round's head is packed; each later round's head is the loose parcel stacked here.
        area.write(&format!("w/{}.txt", what), "new\n");
        area.ok("w", &["load", &format!("{}.txt", what)]);
        if bytes.is_none() {
            area.refused("w", &["stack", "-m", what], "object_unreadable", 57);
        }

        area.ok("w", &["heal"]);
        assert_eq!(std::fs::read(&index).unwrap(), original, "{}: the index is rebuilt as it was written", what);
        assert_eq!(kept_aside(), aside + usize::from(bytes.is_some()), "{}: a replaced index is kept", what);
        area.ok("w", &["history"]);
        area.ok("w", &["stack", "-m", what]);
        area.ok("w", &["stocktake"]);
    }
}

#[test]
fn lost_keys_are_recovered_by_regenesis_and_every_clone_accepts_it_once_named() {
    let area = Area::new("regenesis");
    let _server = two_clones(&area);
    area.ok("a", &["office", "enroll"]);
    area.ok("a", &["lift"]);
    area.ok("b", &["lower"]);
    forklift_core::util::file_utils::rename(area.path("keys"), area.path("lost-keys")).unwrap();

    area.write("a/f.txt", "after\n");
    area.ok("a", &["load", "f.txt"]);
    let refused = area.run("a", &["stack", "-m", "after"]);
    assert!(stderr(&refused).contains("is present on this machine"), "{}", stderr(&refused));

    area.refused("a", &["office", "regenesis"], "confirmation_required", 39);
    area.ok("a", &["office", "regenesis", "--confirm"]);
    area.ok("a", &["stack", "-m", "after"]);
    area.ok("a", &["lift"]);

    let error = area.refused("b", &["lower"], "trust_changed", 22);
    let genesis = error["details"]["current"].as_str().unwrap().to_string();
    assert_eq!(error["details"]["what_changed"], "regenesis", "{}", error);
    area.refused("b", &["office", "accept-regenesis"], "trust_changed", 22);
    area.ok("b", &["office", "accept-regenesis", "--confirm", &genesis]);
    area.ok("b", &["lower"]);
    assert_eq!(std::fs::read_to_string(area.path("b/f.txt")).unwrap(), "after\n");
}

#[test]
fn undo_as_brings_a_deleted_pallet_back_and_park_pop_brings_work_back() {
    let area = Area::new("undo");
    area.prepare("w");
    area.stack("w", "f.txt", "one\n");
    area.ok("w", &["pallet", "create", "x"]);
    area.stack("w", "g.txt", "two\n");
    area.ok("w", &["shift", "main"]);
    area.ok("w", &["pallet", "delete", "x", "--force"]);
    area.ok("w", &["undo", "--as", "rescued"]);
    area.ok("w", &["shift", "rescued"]);
    assert!(Path::new(&area.path("w/g.txt")).exists());

    area.write("w/f.txt", "edited\n");
    area.ok("w", &["load", "f.txt"]);
    area.ok("w", &["park"]);
    assert_eq!(std::fs::read_to_string(area.path("w/f.txt")).unwrap(), "one\n");
    area.ok("w", &["park", "pop"]);
    assert_eq!(std::fs::read_to_string(area.path("w/f.txt")).unwrap(), "edited\n");
}

#[test]
fn a_clone_stopped_by_a_wrong_only_path_finishes_with_the_right_one() {
    let area = Area::new("clone");
    let server = two_clones(&area);
    std::fs::create_dir(area.path("a/src")).unwrap();
    area.stack("a", "src/app.txt", "app\n");
    area.ok("a", &["lift"]);

    for _ in 0..2 {
        let error = area.refused(".", &["clone", &server.url, "partial", "--token", "secret", "--only", "nosuch"],
            "clone_incomplete", 30);
        assert_eq!(error["details"]["cause_code"], "only_prefix_not_found", "{}", error);
        let objects = area.path("partial/.forklift/objects");
        assert!(!objects.exists() || object_files(&objects).is_empty(), "the refused clone stored nothing");
    }

    area.ok(".", &["clone", &server.url, "partial", "--token", "secret", "--only", "src"]);
    assert_eq!(std::fs::read_to_string(area.path("partial/src/app.txt")).unwrap(), "app\n");
}
