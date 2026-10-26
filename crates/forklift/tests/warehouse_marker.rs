//! A directory is a warehouse only when its `.forklift` folder holds the warehouse marker that
//! `prepare` writes last. These tests check that every other `.forklift` is refused as
//! `not_a_warehouse` before anything is written, with a next step that names the root it refused
//! and that works when followed: an empty folder, an unmarked one, a bay whose redirect names
//! something that is not a warehouse, and an empty folder inside a warehouse.

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
        self.run_ok(&root, &["config", "operator.identifier", "marker@forklift"]);
        std::fs::write(root.join("app.txt"), "payload\n").unwrap();
        self.run_ok(&root, &["load", "."]);
        self.run_ok(&root, &["stack", "first"]);
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
    area.run_ok(&root, &["config", "operator.identifier", "marker@forklift"]);
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

    for args in [&["stocktake"][..], &["load", "."], &["prepare"], &["config", "operator.identifier", "x@y"], &["profile", "use", "work"]] {
        let refusal = area.refused(&root, args);
        assert!(refusal.message.contains("holds no warehouse marker"), "{:?}: {}", args, refusal.message);
        assert!(
            refusal.next_step.starts_with(&format!("Move {} aside, run \"forklift prepare\" in {}", quoted(&forklift), quoted(&root))),
            "{:?}: {}", args, refusal.next_step
        );
        assert_eq!(listing(&forklift), before, "{:?} changed the folder", args);
    }

    // The remedy: move it aside and prepare again.
    std::fs::rename(&forklift, root.join(".forklift.old")).unwrap();
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
    let sub = area.dir("w/sub");
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

    for args in [&["load", "."][..], &["config", "operator.identifier", "x@y"], &["profile", "use", "work"]] {
        let refusal = area.refused(&bay, args);
        assert!(refusal.message.contains(&quoted(&root.join(FOLDER_NAME_FORKLIFT_ROOT))), "{:?}: {}", args, refusal.message);
        assert!(refusal.next_step.contains(&format!("run \"forklift prepare\" in {}", quoted(&root))), "{:?}: {}", args, refusal.next_step);
    }
    assert_eq!(listing(&root.join(FOLDER_NAME_FORKLIFT_ROOT)), shared, "the warehouse was written");
    assert_eq!(listing(&bay.join(FOLDER_NAME_FORKLIFT_ROOT)), own, "the bay was written");
}

/// A bay whose warehouse is gone is told to restore it or delete the bay's folder.
#[test]
fn a_bay_whose_warehouse_is_gone_is_told_to_restore_it() {
    let area = Area::new("bay-gone");
    let root = area.warehouse("w");
    let bay = area.path("bay");
    area.run_ok(&root, &["bay", "add", "feature", bay.to_str().unwrap()]);
    std::fs::rename(&root, area.path("w-moved")).unwrap();

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
