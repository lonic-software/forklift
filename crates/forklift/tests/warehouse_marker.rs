//! A directory is a warehouse only when its `.forklift` folder holds the warehouse marker that
//! `prepare` writes last. These tests check that every other `.forklift` is refused as
//! `not_a_warehouse` before anything is written, with a next step that names the root it refused
//! and that works when followed: an empty folder, an unmarked one, a bay whose redirect names
//! something that is not a warehouse, and an empty folder inside a warehouse. A warehouse whose
//! marker records a format this build does not read is a warehouse, and is refused as that, with the
//! way out its condition calls for.

use std::path::{Path, PathBuf};
use std::process::Output;

use forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT;
use forklift_core::util::bay_utils;

mod common;
use common::forklift_command;

/// A test area: a scratch folder with its own global configuration and keys.
struct Area {
    base: PathBuf,
    home: PathBuf,
}

impl Area {
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-marker-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // Canonical, so paths compare with the ones forklift reports (macOS's /var is /private/var),
        // without Windows' verbatim prefix, which a working directory never carries.
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let home = base.join("home");

        Area { base, home }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let dir = self.path(relative);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.run(dir, args);
        assert!(output.status.success(), "{:?} in {:?} failed: {}{}", args, dir, text(&output.stdout), text(&output.stderr));
        output
    }

    /// A prepared warehouse at `relative` with `app.txt` stacked on `main`.
    fn warehouse(&self, relative: &str) -> PathBuf {
        let root = self.dir(relative);
        self.run_ok(&root, &["prepare"]);
        self.run_ok(&root, &["config", "set", "operator.identifier", "marker@forklift"]);
        std::fs::write(root.join("app.txt"), "payload\n").unwrap();
        self.run_ok(&root, &["load", "."]);
        self.run_ok(&root, &["stack", "-m", "first"]);
        root
    }

    /// Run `args` (with `--json`) in `dir` and return the refusal, asserting it is `not_a_warehouse`
    /// with exit code 3.
    fn refused(&self, dir: &Path, args: &[&str]) -> Refusal {
        let output = self.run(dir, &[&["--json"][..], args].concat());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| panic!("{:?}: not one JSON document: {}{}", args, text(&output.stdout), text(&output.stderr)));
        assert_eq!(value["error"]["code"], "not_a_warehouse", "{:?}: {}", args, value);
        assert_eq!(output.status.code(), Some(3), "{:?}: {}", args, value);

        Refusal {
            message: value["error"]["message"].as_str().unwrap().to_string(),
            next_step: value["error"]["next_step"].as_str().unwrap().to_string(),
        }
    }
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct Refusal {
    message: String,
    next_step: String,
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every entry below `dir`, relative, sorted: what a refused command must leave as it was.
fn listing(dir: &Path) -> Vec<String> {
    let mut entries = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            entries.push(path.strip_prefix(dir).unwrap().to_string_lossy().into_owned());
            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                pending.push(path);
            }
        }
    }
    entries.sort();
    entries
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy())
}

/// An empty `.forklift` is no warehouse: load, heal and audit refuse it before writing anything,
/// and the next step, from a folder below it too, names the root to prepare.
#[test]
fn an_empty_forklift_folder_is_refused_before_anything_is_written() {
    let area = Area::new("empty");
    let root = area.dir("plain");
    std::fs::create_dir(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    let deep = area.dir("plain/sub/deep");

    for (dir, args) in [(&root, &["load", "."][..]), (&root, &["heal"]), (&root, &["audit"]), (&deep, &["stocktake"])] {
        let refusal = area.refused(dir, args);
        assert!(refusal.message.contains(&quoted(&root.join(FOLDER_NAME_FORKLIFT_ROOT))), "{:?}: {}", args, refusal.message);
        assert_eq!(refusal.next_step, format!("Run \"forklift prepare\" in {}.", quoted(&root)), "{:?}", args);
        assert_eq!(listing(&root.join(FOLDER_NAME_FORKLIFT_ROOT)), Vec::<String>::new(), "{:?} wrote into it", args);
    }

    // Following the step makes it one.
    area.run_ok(&root, &["prepare"]);
    area.run_ok(&root, &["config", "set", "operator.identifier", "marker@forklift"]);
    area.run_ok(&root, &["load", "."]);
}

/// A `.forklift` with something in it but no marker (made before warehouses were marked) is
/// refused by every command and by prepare; the re-create remedy works.
#[test]
fn an_unmarked_folder_is_refused_and_prepare_will_not_take_it() {
    let area = Area::new("unmarked");
    let root = area.warehouse("old");
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
    std::fs::remove_file(forklift.join("warehouse")).unwrap();
    let before = listing(&forklift);

    for args in [&["stocktake"][..], &["load", "."], &["prepare"], &["config", "set", "operator.identifier", "x@y"], &["profile", "use", "work"]] {
        let refusal = area.refused(&root, args);
        assert!(refusal.message.contains("holds no warehouse marker"), "{:?}: {}", args, refusal.message);
        assert!(
            refusal.next_step.starts_with(&format!("Move {} aside, run \"forklift prepare\" in {}", quoted(&forklift), quoted(&root))),
            "{:?}: {}", args, refusal.next_step
        );
        assert_eq!(listing(&forklift), before, "{:?} changed the folder", args);
    }

    // The remedy: move it aside and prepare again.
    forklift_core::util::file_utils::rename(&forklift, root.join(".forklift.old")).unwrap();
    area.run_ok(&root, &["prepare"]);
    area.run_ok(&root, &["stocktake"]);
}

/// `prepare` on an empty `.forklift` marks it: an empty folder is prepare's to take.
#[test]
fn prepare_takes_an_empty_forklift_folder() {
    let area = Area::new("prepare-empty");
    let root = area.dir("fresh");
    std::fs::create_dir(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

    area.run_ok(&root, &["prepare"]);

    assert!(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("warehouse").is_file());
    area.run_ok(&root, &["stocktake"]);
}

/// An empty `.forklift` inside a warehouse is refused, not skipped, with removing it as the first
/// step; following it makes the folder use the warehouse. A nested one with something in it is
/// refused too.
#[test]
fn an_empty_forklift_folder_inside_a_warehouse_points_at_the_warehouse() {
    let area = Area::new("nested");
    let root = area.warehouse("w");
    let sub = area.dir("w").join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir(sub.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

    let refusal = area.refused(&sub, &["stocktake"]);
    assert_eq!(
        refusal.next_step,
        format!(
            "Remove the empty {} to use the warehouse at {}, or run \"forklift prepare\" in {}.",
            quoted(&sub.join(FOLDER_NAME_FORKLIFT_ROOT)), quoted(&root), quoted(&sub)
        )
    );

    std::fs::remove_dir(sub.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    let status = area.run_ok(&sub, &["--json", "stocktake"]);
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["ok"], true, "{}", value);

    // Not empty: still refused, never skipped for the warehouse above.
    std::fs::create_dir_all(sub.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap();
    let refusal = area.refused(&sub, &["stocktake"]);
    assert!(refusal.message.contains(&quoted(&sub.join(FOLDER_NAME_FORKLIFT_ROOT))), "{}", refusal.message);
}

/// A bay whose warehouse lost its marker is refused, naming that warehouse, and nothing is
/// written in either; `config` and `profile use` in the bay say the same.
#[test]
fn a_bay_of_an_unmarked_warehouse_is_refused_and_nothing_is_written() {
    let area = Area::new("bay-unmarked");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    std::fs::write(bay.join("new.txt"), "new\n").unwrap();
    std::fs::remove_file(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("warehouse")).unwrap();
    let (shared, own) = (listing(&root.join(FOLDER_NAME_FORKLIFT_ROOT)), listing(&bay.join(FOLDER_NAME_FORKLIFT_ROOT)));

    for args in [&["load", "."][..], &["config", "set", "operator.identifier", "x@y"], &["profile", "use", "work"]] {
        let refusal = area.refused(&bay, args);
        assert_eq!(
            refusal.message,
            format!("The bay {} belongs to {}, whose \".forklift\" holds no warehouse marker.", quoted(&bay), quoted(&root)),
            "{:?}", args
        );
        // Prepare's remedy alone would leave a warehouse that does not know the bay.
        assert_eq!(
            refusal.next_step,
            format!(
                "Re-create the warehouse at {} (move its \".forklift\" aside, run \"forklift prepare\" there, then \
                import the history again with \"forklift git import\" or clone it again), then move {} aside and add \
                the bay again with \"forklift bay add\" in {}.",
                quoted(&root), quoted(&bay), quoted(&root)
            ),
            "{:?}", args
        );
    }
    assert_eq!(listing(&root.join(FOLDER_NAME_FORKLIFT_ROOT)), shared, "the warehouse was written");
    assert_eq!(listing(&bay.join(FOLDER_NAME_FORKLIFT_ROOT)), own, "the bay was written");

    // The steps give a working bay (the history coming back as a fresh stack of the same files).
    forklift_core::util::file_utils::rename(root.join(FOLDER_NAME_FORKLIFT_ROOT), root.join(".forklift.old")).unwrap();
    area.run_ok(&root, &["prepare"]);
    area.run_ok(&root, &["config", "set", "operator.identifier", "marker@forklift"]);
    area.run_ok(&root, &["load", "app.txt"]);
    area.run_ok(&root, &["stack", "-m", "again"]);
    forklift_core::util::file_utils::rename(&bay, area.path("bay.old")).unwrap();
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    area.run_ok(&bay, &["stocktake"]);
}

/// A bay in the older format is refused as no warehouse, with the conversion as its way out.
#[test]
fn an_old_format_bay_is_refused_as_no_warehouse() {
    let area = Area::new("bay-old-format");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    let forklift = bay.join(FOLDER_NAME_FORKLIFT_ROOT);

    std::fs::remove_dir_all(&forklift).unwrap();
    std::fs::write(&forklift, format!("{}\n", root.to_string_lossy())).unwrap();
    let refusal = area.refused(&bay, &["stocktake"]);
    assert!(refusal.message.contains("written by an older version of Forklift"), "{}", refusal.message);
    assert!(refusal.next_step.starts_with("Convert it in place: "), "{}", refusal.next_step);
}

/// An empty `.forklift` inside a bay is never sent to the bay as if it were the warehouse: the
/// outer working tree is named only when it is a warehouse.
#[test]
fn an_empty_forklift_folder_inside_a_bay_names_no_outer_warehouse() {
    let area = Area::new("nested-bay");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    let sub = bay.join("sub");
    std::fs::create_dir_all(sub.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

    let refusal = area.refused(&sub, &["stocktake"]);
    assert_eq!(refusal.next_step, format!("Run \"forklift prepare\" in {}.", quoted(&sub)));
}

/// A marker this build cannot read is refused by every command and by `prepare`, never answered
/// with "prepare" and never read as a full warehouse: one with no mode, two, a mode this build
/// does not know, or the version 1 marker an older Forklift wrote before the marker recorded the
/// mode, whose refusal says so.
///
/// Revert: read a v1 marker, or a missing mode, as `full` → stocktake succeeds → red.
#[test]
fn an_unreadable_marker_is_not_answered_with_prepare() {
    let area = Area::new("unreadable");
    let root = area.warehouse("w");
    let marker = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("warehouse");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "/forklift-warehouse 3\nformat 1\nfull\n");

    let old_records = ["/forklift-warehouse 1\n", "/forklift-warehouse 2\nfull\n"];
    for bad in ["/forklift-warehouse 3\n", "/forklift-warehouse 3\nfull\n", "/forklift-warehouse 3\nformat 1\n",
                "/forklift-warehouse 3\nformat 1\nfull\ncache\n", "/forklift-warehouse 3\nformat one\nfull\n",
                "/forklift-warehouse 3\nformat 01\nfull\n", "/forklift-warehouse 3\nformat 1\nshallow\n",
                "/forklift-warehouse 3\nformat 1\nfull\nupgrading 2\n", "/forklift-warehouse 99\nformat 1\nfull\n",
                old_records[0], old_records[1]] {
        std::fs::write(&marker, bad).unwrap();
        let before = listing(&root);

        for args in [&["stocktake"][..], &["stack", "-m", "again"], &["prepare"]] {
            let output = area.run(&root, &[&["--json"][..], args].concat());
            assert!(!output.status.success(), "{:?} over {:?}", args, bad);
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_ne!(value["error"]["code"], "not_a_warehouse", "{}", value);
            let message = value["error"]["message"].as_str().unwrap();
            assert!(message.starts_with("The warehouse marker "), "{}", value);
            assert_eq!(message.contains("older version of Forklift"), old_records.contains(&bad), "{}", value);
            assert!(!value["error"]["next_step"].as_str().unwrap_or("").contains("prepare"), "{}", value);
        }
        assert_eq!(listing(&root), before, "{:?}: a refused command writes nothing", bad);
    }
}

/// A full warehouse holds no fetch scope, so one placed there by hand is refused rather than
/// read as the scope of a sparse warehouse (or ignored): commands that read the fetch scope say
/// the warehouse is recorded as full, and deleting the file is the way out.
///
/// Revert: let a full warehouse return its fetch-scope file (or ignore it) → the stack lands → red.
#[test]
fn a_full_warehouse_with_a_fetch_scope_is_refused() {
    let area = Area::new("stray-fetch-scope");
    let root = area.warehouse("w");
    let config = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("config");
    std::fs::write(config.join("fetch-scope"), "/forklift-scope 3\napp\n").unwrap();
    std::fs::write(root.join("app.txt"), "changed\n").unwrap();

    // Load reads the main tree's scope, which is checked against the mode too, so it refuses as
    // stack does.
    for args in [&["load", "."][..], &["stack", "-m", "second"]] {
        let output = area.run(&root, &[&["--json"][..], args].concat());
        assert!(!output.status.success(), "{:?}: {}", args, text(&output.stdout));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("recorded as full but holds a fetch scope"), "{:?}: {}", args, value);
        assert!(message.contains("Delete that file"), "{:?}: {}", args, value);
    }
    let log = area.run_ok(&root, &["--json", "log"]);
    assert!(!text(&log.stdout).contains("second"), "the stack must not land: {}", text(&log.stdout));

    // The named way out works.
    std::fs::remove_file(config.join("fetch-scope")).unwrap();
    area.run_ok(&root, &["load", "."]);
    area.run_ok(&root, &["stack", "-m", "second"]);
}

/// `forklift-server prepare` prints a refusal as its message and next step, never the frame
/// core carries it in.
#[test]
fn the_server_prints_a_refusal_without_its_frame() {
    let area = Area::new("server-prepare");
    let old = area.dir("old");
    std::fs::create_dir_all(old.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap();

    let output = std::process::Command::new(common::server_binary())
        .args(["prepare", "--root", old.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = text(&output.stderr);
    // The root as the server resolved it (canonical: on Windows, with the verbatim prefix).
    let (message, next_step) = stderr.split_once('\n').expect("a message line");
    assert!(message.starts_with('"') && message.ends_with(&format!(
        "{}\" holds no warehouse marker: it was made before warehouses were marked, or is not a warehouse.",
        std::path::MAIN_SEPARATOR_STR.to_string() + FOLDER_NAME_FORKLIFT_ROOT
    )), "{:?}", stderr);
    assert!(next_step.starts_with("Move "), "{:?}", stderr);
    assert!(!stderr.contains('\u{1f}') && !stderr.contains("not_a_warehouse"), "{:?}", stderr);
}

/// A bay whose warehouse is gone is told to restore it or delete the bay's folder.
#[test]
fn a_bay_whose_warehouse_is_gone_is_told_to_restore_it() {
    let area = Area::new("bay-gone");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    forklift_core::util::file_utils::rename(&root, area.path("w-moved")).unwrap();

    let refusal = area.refused(&bay, &["stocktake"]);
    assert_eq!(
        refusal.next_step,
        format!("Restore the warehouse at {}, or delete {}.", quoted(&root), quoted(&bay.join(FOLDER_NAME_FORKLIFT_ROOT)))
    );
}

/// A bay whose redirect was removed is no bay and no warehouse: refused.
#[test]
fn a_bay_without_its_redirect_is_refused() {
    let area = Area::new("bay-no-redirect");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    std::fs::remove_file(bay.join(FOLDER_NAME_FORKLIFT_ROOT).join("redirect")).unwrap();

    let refusal = area.refused(&bay, &["stocktake"]);
    assert!(refusal.message.contains(&quoted(&bay.join(FOLDER_NAME_FORKLIFT_ROOT))), "{}", refusal.message);
}

/// `bay add` run inside a bay makes a bay of the warehouse, never one that redirects to the bay.
#[test]
fn a_bay_added_inside_a_bay_belongs_to_the_warehouse() {
    let area = Area::new("bay-in-bay");
    let root = area.warehouse("w");
    let first = area.path("first");
    area.run_ok(&root, &["bay", "add", "first", first.to_str().unwrap()]);
    let second = area.path("second");

    area.run_ok(&first, &["bay", "add", "second", second.to_str().unwrap()]);

    let redirect = bay_utils::read_bay_redirect(&second.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    assert_eq!(redirect.warehouse_root, root);
    assert_eq!(std::fs::read_to_string(second.join("app.txt")).unwrap(), "payload\n");
    area.run_ok(&second, &["stocktake"]);
}

/// A bay whose redirect names another bay (what `bay add` inside a bay once wrote) is refused
/// without following the chain, and the steps it names give a working bay.
#[test]
fn a_bay_redirecting_to_a_bay_is_refused_and_its_steps_give_a_working_bay() {
    let area = Area::new("bay-to-bay");
    let root = area.warehouse("w");
    let first = area.path("first");
    area.run_ok(&root, &["bay", "add", "first", first.to_str().unwrap()]);
    let chained = area.path("chained");
    area.run_ok(&root, &["bay", "add", "chained", chained.to_str().unwrap()]);
    // The old state: the bay is registered in the warehouse, its redirect names the other bay.
    std::fs::remove_dir_all(chained.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    bay_utils::write_bay_redirect(&chained, &first, "chained").unwrap();

    let refusal = area.refused(&chained, &["stocktake"]);
    assert!(refusal.message.contains("which is another bay"), "{}", refusal.message);
    assert_eq!(
        refusal.next_step,
        format!(
            "Run \"forklift bay remove chained\" in {}, delete {}, then run \"forklift bay add\" there under a new \
            name into an empty folder.",
            quoted(&root), quoted(&chained.join(FOLDER_NAME_FORKLIFT_ROOT))
        )
    );

    // The registration goes; the folder is left, as not this bay's, for the next step to delete.
    let removed = area.run(&root, &["bay", "remove", "chained"]);
    assert!(text(&removed.stderr).contains("The bay \"chained\" was removed."), "{}", text(&removed.stderr));
    std::fs::remove_dir_all(chained.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    let fresh = area.path("fresh");
    area.run_ok(&root, &["bay", "add", "renewed", fresh.to_str().unwrap()]);
    area.run_ok(&fresh, &["stocktake"]);
}

/// `forklift-server serve --root` refuses a root whose `.forklift` holds no marker, with this
/// server's own remedy: its prepare for an empty folder, and making the warehouse again for one
/// with something in it.
#[test]
fn the_server_serves_no_unmarked_root() {
    let area = Area::new("server-serve-unmarked");
    let empty = area.dir("empty");
    std::fs::create_dir_all(empty.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
    let old = area.dir("old");
    std::fs::create_dir_all(old.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap();

    for (root, expected) in [(&empty, "Run \"forklift-server prepare\" with --root "), (&old, "Move ")] {
        let before = listing(root);
        // Waited on with a deadline: a server that took the folder for a warehouse would serve it forever.
        let mut serving = std::process::Command::new(common::server_binary())
            .args(["serve", "--root", root.to_str().unwrap(), "--addr", "127.0.0.1:0", "--open"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while serving.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = serving.kill();
                let _ = serving.wait();
                panic!("forklift-server served {:?}, which holds no warehouse marker", root);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let served = serving.wait_with_output().unwrap();
        assert!(!served.status.success());
        let stderr = text(&served.stderr);
        let (_, next_step) = stderr.trim_end().split_once('\n').expect("a message line");
        assert!(next_step.starts_with(expected), "{:?}", stderr);
        assert!(!stderr.contains("\"forklift prepare\"") && !stderr.contains("git import"), "{:?}", stderr);
        assert_eq!(listing(root), before, "refused before any lock is taken");
    }
}

/// Run `args` (with `--json`) in `dir`, assert the refusal's code and exit, and return its next step.
fn format_refusal(area: &Area, dir: &Path, args: &[&str], code: &str, exit: i32) -> (String, String) {
    let output = area.run(dir, &[&["--json"][..], args].concat());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{:?}: not one JSON document: {}{}", args, text(&output.stdout), text(&output.stderr)));
    assert_eq!(value["error"]["code"], code, "{:?}: {}", args, value);
    assert_eq!(output.status.code(), Some(exit), "{:?}: {}", args, value);

    (value["error"]["message"].as_str().unwrap().to_string(), value["error"]["next_step"].as_str().unwrap_or("").to_string())
}

fn marker_of(root: &Path) -> PathBuf {
    root.join(FOLDER_NAME_FORKLIFT_ROOT).join("warehouse")
}

/// Hold the warehouse's format lock exclusively, as an upgrade does.
fn hold_format_exclusively(root: &Path) -> std::fs::File {
    let file = std::fs::File::open(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("format.lock")).unwrap();
    file.try_lock().unwrap();
    file
}

/// A warehouse in any format but this build's is refused before anything is read or written, with
/// the remedy its side of the difference calls for, and a resume is never answered as a plain
/// difference: the upgrade on record outranks the format.
///
/// Revert: let `judge_format` pass another format → the stocktake reads it → red.
#[test]
fn a_warehouse_in_another_format_is_refused_with_its_own_way_out() {
    let area = Area::new("format-refusals");
    let root = area.warehouse("w");
    let marker = marker_of(&root);
    let cases = [
        // Older than any format this build upgrades: made again, never sent to the upgrade.
        ("/forklift-warehouse 3\nformat 0\nfull\n", "warehouse_format_older", 40, "\"forklift prepare\""),
        ("/forklift-warehouse 3\nformat 2\nfull\n", "warehouse_format_newer", 41, "Update this Forklift"),
        // A mode of a format this build does not read is not this build's to judge.
        ("/forklift-warehouse 3\nformat 2\nsomething-new\n", "warehouse_format_newer", 41, "Update this Forklift"),
        ("/forklift-warehouse 3\nformat 1\nfull\nupgrading 2 3\n", "warehouse_upgrade_incomplete", 42, "forklift upgrade"),
        ("/forklift-warehouse 3\nformat 0\nfull\nupgrading 1 0\n", "warehouse_upgrade_incomplete", 42, "forklift upgrade"),
    ];

    for (content, code, exit, expected) in cases {
        std::fs::write(&marker, content).unwrap();
        let before = listing(&root);

        for args in [&["stocktake"][..], &["stack", "-m", "again"], &["prepare"], &["pallet", "list"]] {
            let (_, next_step) = format_refusal(&area, &root, args, code, exit);
            assert!(next_step.contains(expected), "{:?} over {:?}: {}", args, content, next_step);
            assert!(expected.contains("prepare") || !next_step.contains("prepare"), "{:?} over {:?}: {}", args, content, next_step);
        }
        assert_eq!(listing(&root), before, "{:?}: a refused command writes nothing", content);
    }
}

/// A bay works on its warehouse's format, so it answers to that warehouse's marker through its
/// redirect, and its next step names the warehouse, which is what an upgrade runs on.
///
/// Revert: judge a bay by its own state folder → the bay's stocktake runs → red.
#[test]
fn a_bay_is_refused_on_its_warehouses_format() {
    let area = Area::new("format-bay");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    area.run_ok(&bay, &["stocktake"]);

    std::fs::write(marker_of(&root), "/forklift-warehouse 3\nformat 1\nfull\nupgrading 2 0\n").unwrap();
    let (_, next_step) = format_refusal(&area, &bay, &["stocktake"], "warehouse_upgrade_incomplete", 42);
    assert!(next_step.contains(&format!("\"forklift upgrade\" in \"{}\"", root.display())), "{}", next_step);

    std::fs::write(marker_of(&root), "/forklift-warehouse 3\nformat 2\nfull\n").unwrap();
    format_refusal(&area, &bay, &["stocktake"], "warehouse_format_newer", 41);
}

/// While an upgrade holds the format lock a command refuses at once rather than wait for it, and
/// runs again as soon as the lock is let go.
///
/// Revert: let the open go on when the lock is held → the stocktake runs under the upgrade → red.
#[test]
fn a_command_refuses_at_once_while_an_upgrade_holds_the_format() {
    let area = Area::new("format-running");
    let root = area.warehouse("w");
    let held = hold_format_exclusively(&root);

    let started = std::time::Instant::now();
    format_refusal(&area, &root, &["stocktake"], "warehouse_upgrade_running", 43);
    assert!(started.elapsed() < std::time::Duration::from_secs(30), "waited {:?}", started.elapsed());

    drop(held);
    area.run_ok(&root, &["stocktake"]);
}

/// `prepare` writes the format lock, and a warehouse that lost it gets it back when it can be
/// written. One that cannot is refused for what it is, not read without a lease.
///
/// Revert: treat a missing sidecar as no upgrade running, without recreating it → it stays gone → red.
#[test]
#[cfg(unix)]
fn a_lost_format_lock_is_made_again_or_refused() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("format-sidecar");
    let root = area.warehouse("w");
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
    let lock = forklift.join("format.lock");
    assert!(lock.exists(), "prepare writes the format lock");

    std::fs::remove_file(&lock).unwrap();
    area.run_ok(&root, &["stocktake"]);
    assert!(lock.exists(), "the first command makes it again");

    std::fs::remove_file(&lock).unwrap();
    std::fs::set_permissions(&forklift, std::fs::Permissions::from_mode(0o555)).unwrap();
    let refused = area.run(&root, &["--json", "pallet", "list"]);
    std::fs::set_permissions(&forklift, std::fs::Permissions::from_mode(0o755)).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(value["error"]["code"], "warehouse_lease_missing", "{}", value);
    assert_eq!(refused.status.code(), Some(44), "{}", value);
}

/// A warehouse that cannot be written can still be read: the lease needs no write access to a lock
/// that is already there.
///
/// Revert: open the sidecar for writing → permission denied on a read-only warehouse → red.
#[test]
#[cfg(unix)]
fn a_read_only_warehouse_still_takes_the_lease() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("format-read-only");
    let root = area.warehouse("w");
    let lock = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("format.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).unwrap();

    let opened = forklift_core::util::lock_utils::FormatLease::shared(&root.join(FOLDER_NAME_FORKLIFT_ROOT));
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    opened.unwrap();
}

/// `prepare` over a warehouse in another format neither joins it nor adds to it.
///
/// Revert: let `check_preparable` read only the marker's presence → `prepare` answers "already" → red.
#[test]
fn prepare_refuses_a_warehouse_in_another_format_and_adds_nothing() {
    let area = Area::new("format-prepare");
    let root = area.warehouse("w");
    std::fs::write(marker_of(&root), "/forklift-warehouse 3\nformat 2\nfull\n").unwrap();
    let before = listing(&root);

    format_refusal(&area, &root, &["prepare"], "warehouse_format_newer", 41);
    assert_eq!(listing(&root), before);
    assert_eq!(std::fs::read_to_string(marker_of(&root)).unwrap(), "/forklift-warehouse 3\nformat 2\nfull\n");
}

/// A marker from before the marker recorded a format is told it has no upgrade, whichever older
/// version wrote it.
///
/// Revert: answer an old marker with the older-format refusal → the message names an upgrade → red.
#[test]
fn a_marker_from_before_formats_is_told_to_make_the_warehouse_again() {
    let area = Area::new("format-record-v2");
    let root = area.warehouse("w");

    for old in ["/forklift-warehouse 2\nfull\n", "/forklift-warehouse 1\n"] {
        std::fs::write(marker_of(&root), old).unwrap();
        let output = area.run(&root, &["--json", "stocktake"]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("Make the warehouse again"), "{}", value);
        assert!(message.contains("No upgrade reaches back"), "{}", value);
    }
}

/// The server refuses a warehouse in another format with the server's own remedy, before serving.
///
/// Revert: drop the format judgment from the server's open → the server serves it → the deadline fires → red.
#[test]
fn the_server_serves_no_warehouse_in_another_format() {
    let area = Area::new("format-server");
    let root = area.warehouse("w");

    for (content, expected) in [("/forklift-warehouse 3\nformat 0\nfull\n", "\"forklift-server prepare\""),
                                ("/forklift-warehouse 3\nformat 2\nfull\n", "Update this Forklift")] {
        std::fs::write(marker_of(&root), content).unwrap();
        let mut serving = std::process::Command::new(common::server_binary())
            .args(["serve", "--root", root.to_str().unwrap(), "--addr", "127.0.0.1:0", "--open"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while serving.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                let _ = serving.kill();
                let _ = serving.wait();
                panic!("forklift-server served a warehouse marked {:?}", content);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let served = serving.wait_with_output().unwrap();
        assert!(!served.status.success());
        assert!(text(&served.stderr).contains(expected), "{:?}: {}", content, text(&served.stderr));
    }
}

/// Every other way the server takes a root refuses another format too, and a running server keeps
/// an upgrade out for as long as it serves.
///
/// Revert: let the handle drop its lease → the exclusive lock lands under the running server → red.
#[test]
fn the_server_holds_the_format_while_it_serves_and_its_other_commands_refuse() {
    use std::io::BufRead;

    let area = Area::new("format-server-held");
    let root = area.warehouse("w");
    let lock = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("format.lock");

    let mut serving = std::process::Command::new(common::server_binary())
        .args(["serve", "--root", root.to_str().unwrap(), "--addr", "127.0.0.1:0", "--open"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(serving.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let during = std::fs::File::open(&lock).unwrap().try_lock().is_ok();
    let _ = serving.kill();
    let _ = serving.wait();
    assert!(!line.is_empty(), "the server started");
    assert!(!during, "an upgrade could start under a running server");
    std::fs::File::open(&lock).unwrap().try_lock().unwrap();

    std::fs::write(marker_of(&root), "/forklift-warehouse 3\nformat 2\nfull\n").unwrap();
    for args in [&["gc", "--root"][..], &["bundle", "--root"]] {
        let output = std::process::Command::new(common::server_binary())
            .args(args).arg(&root).output().unwrap();
        assert!(!output.status.success(), "{:?}", args);
        assert!(text(&output.stderr).contains("Update this Forklift"), "{:?}: {}", args, text(&output.stderr));
    }
}

/// `prepare` over a complete warehouse whose format lock this user cannot write has nothing to do,
/// and is not refused for a lock that is there.
///
/// Revert: open the format lock for writing whenever prepare fills → `warehouse_lease_missing` → red.
#[test]
#[cfg(unix)]
fn prepare_over_a_warehouse_with_a_read_only_format_lock_has_nothing_to_do() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("format-prepare-read-only");
    let root = area.warehouse("w");
    let lock = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("format.lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).unwrap();

    let output = area.run(&root, &["prepare"]);
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(output.status.success(), "{}{}", text(&output.stdout), text(&output.stderr));
}

/// `upgrade` on a warehouse already in this build's format changes nothing and says so, from
/// anywhere inside the working tree.
///
/// Revert: dispatch `upgrade` through the open gate → it holds the lease itself and refuses → red.
#[test]
fn upgrade_leaves_a_current_warehouse_as_it_is() {
    let area = Area::new("upgrade-current");
    let root = area.warehouse("w");
    let below = area.dir("w/src");
    let before = std::fs::read(marker_of(&root)).unwrap();

    let output = area.run_ok(&below, &["--json", "upgrade"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"], serde_json::json!({"from": 1, "to": 1, "resumed": null, "steps_run": 0}), "{}", value);

    let output = area.run_ok(&root, &["upgrade"]);
    assert!(text(&output.stdout).contains("Nothing to do."), "{}", text(&output.stdout));
    assert_eq!(std::fs::read(marker_of(&root)).unwrap(), before);
    area.run_ok(&root, &["stocktake"]);
}

/// `upgrade` needs the warehouse to itself: while another process holds the format lease it
/// refuses at once with its own code, and runs once the lease is let go.
///
/// Revert: take the lease shared in the upgrade → it runs beside the reader → red.
#[test]
fn upgrade_refuses_a_warehouse_in_use() {
    let area = Area::new("upgrade-in-use");
    let root = area.warehouse("w");
    let held = forklift_core::util::lock_utils::FormatLease::shared(&root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

    let (message, next_step) = format_refusal(&area, &root, &["upgrade"], "warehouse_in_use", 45);
    assert!(message.contains("is in use"), "{}", message);
    assert!(next_step.contains("run the upgrade again"), "{}", next_step);

    drop(held);
    area.run_ok(&root, &["upgrade"]);
}

/// A bay is upgraded with its warehouse, so `upgrade` in a bay names the warehouse to run it in.
///
/// Revert: let `upgrade` follow the bay's redirect → it upgrades from the bay → red.
#[test]
fn upgrade_in_a_bay_names_its_warehouse() {
    let area = Area::new("upgrade-bay");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);

    let refusal = area.refused(&bay, &["upgrade"]);
    assert_eq!(refusal.next_step, format!("Run \"forklift upgrade\" in {}.", quoted(&root)));
}

/// What this build's upgrade cannot carry is refused with the way out its side calls for, and the
/// marker is left as it was: a format older than any upgrade reaches is made again, a newer one or
/// an unfinished upgrade to a newer one needs a newer binary.
///
/// Revert: give the older-format refusal the gate's next step → the upgrade names itself → red.
#[test]
fn upgrade_refuses_what_it_cannot_carry_and_says_where_to_go() {
    let area = Area::new("upgrade-refusals");
    let root = area.warehouse("w");
    let cases = [
        ("/forklift-warehouse 3\nformat 0\nfull\n", "warehouse_format_older", 40, "\"forklift prepare\""),
        ("/forklift-warehouse 3\nformat 2\nfull\n", "warehouse_format_newer", 41, "Update this Forklift"),
        ("/forklift-warehouse 3\nformat 1\nfull\nupgrading 2 0\n", "warehouse_upgrade_incomplete", 42, "reads format 2"),
    ];

    for (content, code, exit, expected) in cases {
        std::fs::write(marker_of(&root), content).unwrap();
        let (_, next_step) = format_refusal(&area, &root, &["upgrade"], code, exit);
        assert!(next_step.contains(expected), "{:?}: {}", content, next_step);
        assert!(!next_step.contains("forklift upgrade"), "{:?}: {}", content, next_step);
        assert_eq!(std::fs::read_to_string(marker_of(&root)).unwrap(), content);
    }
}

/// `forklift-server upgrade` is refused while the warehouse is served and runs once the server has
/// stopped, and its refusals carry the server's own way out.
///
/// Revert: take the lease shared in the upgrade → it runs under the running server → red.
#[test]
fn the_server_upgrades_only_a_warehouse_it_is_not_serving() {
    use std::io::BufRead;

    let area = Area::new("upgrade-server");
    let root = area.warehouse("w");
    let upgrade = || std::process::Command::new(common::server_binary())
        .args(["upgrade", "--root"]).arg(&root).output().unwrap();

    let mut serving = std::process::Command::new(common::server_binary())
        .args(["serve", "--root", root.to_str().unwrap(), "--addr", "127.0.0.1:0", "--open"])
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(serving.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let during = upgrade();
    let _ = serving.kill();
    let _ = serving.wait();
    assert!(!line.is_empty(), "the server started");
    assert!(!during.status.success(), "the upgrade ran under the server: {}", text(&during.stdout));
    assert!(text(&during.stderr).contains("is in use"), "{}", text(&during.stderr));

    let after = upgrade();
    assert!(after.status.success(), "{}", text(&after.stderr));
    assert!(text(&after.stdout).contains("Nothing to do."), "{}", text(&after.stdout));

    std::fs::write(marker_of(&root), "/forklift-warehouse 3\nformat 0\nfull\n").unwrap();
    let older = upgrade();
    assert!(!older.status.success());
    assert!(text(&older.stderr).contains("\"forklift-server prepare\""), "{}", text(&older.stderr));
}
