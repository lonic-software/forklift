//! `docs/guide/recovery.md` names a way out of each refusal. These tests make the refused state with
//! the real binaries, and run the way out the page names, so a remedy cannot go in on faith or stop
//! working unnoticed. A refusal another suite already drives to its way out is not repeated here.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

mod common;
use common::{forklift_command, repository_root, server_binary};

struct Area {
    root: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let root = std::env::temp_dir().join(format!("forklift-recovery-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
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
        let _ = std::fs::remove_dir_all(&self.root);
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
    child: Child,
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

    fn serve(root: &str) -> Server {
        let mut child = Command::new(server_binary())
            .args(["serve", "--token", "secret", "--root", root, "--addr", "127.0.0.1:0"])
            .env("FORKLIFT_FSYNC", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        Server { child, url }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

#[test]
fn a_lock_a_killed_process_left_is_cleared_by_removing_the_file_it_names() {
    let area = Area::new("lock");
    area.prepare("w");
    area.write("w/a.txt", "a\n");
    let lock = area.path("w/.forklift/lock");
    std::fs::write(&lock, "999999").unwrap();

    let error = area.refused("w", &["load", "a.txt"], "warehouse_locked", 6);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains(".forklift") && message.contains("999999"), "{}", error);

    std::fs::remove_file(&lock).unwrap();
    area.ok("w", &["load", "a.txt"]);
}

#[test]
fn a_serve_lock_a_hard_kill_left_is_cleared_by_removing_the_file_it_names() {
    let area = Area::new("serve-lock");
    let mut server = Server::start(&area, "server");
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    let root = area.path("server");
    let root = root.to_str().unwrap();
    let lock = area.path("server/.forklift/serve.lock");
    assert!(lock.exists(), "a hard kill leaves the lock");

    let gc = Command::new(server_binary()).args(["gc", "--root", root]).output().unwrap();
    assert!(!gc.status.success(), "{}", stdout(&gc));
    assert!(stderr(&gc).contains("serve.lock"), "{}", stderr(&gc));

    std::fs::remove_file(&lock).unwrap();
    let gc = Command::new(server_binary()).args(["gc", "--root", root]).output().unwrap();
    assert!(gc.status.success(), "{}", stderr(&gc));
    let _restarted = Server::serve(root);
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
        std::fs::remove_file(object).unwrap();
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
    let next_step = error["next_step"].as_str().unwrap();
    let (_, rest) = next_step.split_once("pallet create ").unwrap();
    let mut words = rest.split(['"', ' ', ';']);
    let (pallet, hash) = (words.next().unwrap(), words.next().unwrap());

    area.ok("b", &["pallet", "create", pallet, hash]);
    area.ok("b", &["shift", "main"]);
    // A conflict stops the consolidation, which still exits 0.
    assert!(area.ok("b", &["consolidate", pallet]).contains("conflict"));
    area.write("b/f.txt", "both\n");
    area.ok("b", &["load", "f.txt"]);
    area.ok("b", &["stack", "-m", "both"]);
    area.ok("b", &["lift"]);

    area.ok("a", &["lower"]);
    assert_eq!(std::fs::read_to_string(area.path("a/f.txt")).unwrap(), "both\n");
}

#[test]
fn a_quarantined_pack_comes_back_from_its_file_or_a_fresh_clone_and_not_from_heal_or_lower() {
    let area = Area::new("quarantine");
    let server = two_clones(&area);
    area.ok("a", &["store", "compact", "--all"]);
    let pack = area.only_pack("a").with_extension("pack");
    let saved = area.path("saved.pack");
    forklift_core::util::file_utils::rename(&pack, &saved).unwrap();

    let status: serde_json::Value = serde_json::from_str(&area.ok("a", &["--json", "store", "status"])).unwrap();
    assert_eq!(status["data"]["quarantined_packs"].as_array().unwrap().len(), 1, "{}", status);

    assert!(!area.run("a", &["stocktake"]).status.success(), "the store is missing what it needs");
    for not_a_way_out in [&["heal"][..], &["lower"][..]] {
        area.ok("a", not_a_way_out);
        assert!(!area.run("a", &["stocktake"]).status.success(), "{:?} would be a way out", not_a_way_out);
    }

    area.ok(".", &["clone", &server.url, "fresh", "--token", "secret"]);
    area.ok("fresh", &["stocktake"]);

    forklift_core::util::file_utils::rename(&saved, &pack).unwrap();
    area.ok("a", &["stocktake"]);
}

#[test]
fn a_damaged_pack_is_salvaged_and_an_unenumerable_index_is_moved_aside() {
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

    let mut unparsable = original.clone();
    unparsable[0] ^= 0xff;
    std::fs::write(&index, &unparsable).unwrap();
    assert_eq!(status("unenumerable_indexes"), 1);
    let aside = area.path("aside.idx");
    forklift_core::util::file_utils::rename(&index, &aside).unwrap();
    assert_eq!(status("unenumerable_indexes"), 0);
    assert!(!area.run("w", &["history"]).status.success(), "the page warns this index may hold what is needed");
    forklift_core::util::file_utils::rename(&aside, &index).unwrap();
    assert_eq!(status("unenumerable_indexes"), 1, "putting it back restores what was there");

    // The damaged half starts from the good index; a real warehouse would clone afresh here, which
    // the quarantine test runs.
    std::fs::write(&index, &original).unwrap();
    area.ok("w", &["history"]);

    let mut damaged = original.clone();
    let near_end = damaged.len() - 40;
    damaged[near_end] ^= 0xff;
    std::fs::write(&index, &damaged).unwrap();
    assert_eq!(status("damaged_packs"), 1);
    area.ok("w", &["store", "compact", "--all"]);
    assert_eq!(status("damaged_packs"), 0);
    area.ok("w", &["stocktake"]);
    area.ok("w", &["history"]);
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
        assert!(!object_files(&area.path("partial/.forklift/objects")).is_empty(), "what it fetched is kept");
    }

    area.ok(".", &["clone", &server.url, "partial", "--token", "secret", "--only", "src"]);
    assert_eq!(std::fs::read_to_string(area.path("partial/src/app.txt")).unwrap(), "app\n");
}
