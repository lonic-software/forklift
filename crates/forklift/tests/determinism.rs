//! Parallel-determinism + real two-process lock behaviour, part of the hardening test spine.
//!
//! Forklift fans out the staging walk, the tree build and hashing over a worker pool sized to the
//! core count, and packs the object store. Two properties have to survive that:
//!
//!  * **Determinism.** Independent, parallel work reassembles into a fixed order (sorted change
//!    lists, `BTreeMap` trees), so the *content* addresses it produces must not depend on how the
//!    workers were scheduled. Two identical worktrees must yield the same tree hash, a repeated
//!    read must be byte-identical, and a steady-state repack (its layout-derived pack id) must
//!    reproduce the same pack files rather than churn their names.
//!  * **Mutual exclusion across processes.** The warehouse lock is an on-disk sentinel, so a
//!    second process that finds it held must be refused — while a read-only command is unaffected.
//!
//! (The serve-root lock's two-process behaviour — `gc`/second-server refused, `bundle` allowed —
//! is covered end-to-end in `remote.rs`; this file covers the warehouse lock and determinism.)

use std::path::PathBuf;
use std::process::{Command, Output};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// One isolated warehouse with its own home for global config + keys.
struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base = std::env::temp_dir().join(format!("forklift-determinism-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Warehouse { root, home }
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);
        command
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(),
            "`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        output
    }

    /// Run at an arbitrary directory (e.g. a bay outside the warehouse root), keeping the
    /// test's isolated home for global config + keys.
    fn run_ok_at(&self, dir: &PathBuf, args: &[&str]) -> Output {
        let output = Command::new(FORKLIFT)
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap();
        assert!(output.status.success(),
            "`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        output
    }

    fn prepare(&self) {
        self.run_ok(&["prepare"]);
        self.run_ok(&["config", "operator.name", "determinism@forklift"]);
        self.run_ok(&["config", "operator.identifier", "determinism@forklift"]);
    }

    fn head(&self) -> String {
        std::fs::read_to_string(self.root.join(".forklift").join("pallets").join("main"))
            .unwrap().trim().to_string()
    }

    /// The pallet head's tree hash — pure content, no timestamp, so it is deterministic (unlike the
    /// parcel hash, which embeds the wall clock).
    fn head_tree_hash(&self) -> String {
        let output = self.run_ok(&["--json", "peek", &self.head()]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["data"]["tree"].as_str().unwrap().to_string()
    }

    /// The root tree hash a named pallet's head parcel commits (a shared ref, read from the
    /// warehouse; deterministic content, no wall clock).
    fn parcel_tree_hash(&self, pallet: &str) -> String {
        let head = std::fs::read_to_string(self.root.join(".forklift").join("pallets").join(pallet))
            .unwrap().trim().to_string();
        let output = self.run_ok(&["--json", "peek", &head]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["data"]["tree"].as_str().unwrap().to_string()
    }

    fn pack_dir(&self) -> PathBuf {
        self.root.join(".forklift").join("objects").join("pack")
    }

    /// A filename → bytes snapshot of every pack file, so two repacks can be compared byte for byte.
    fn pack_snapshot(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut snapshot = std::collections::BTreeMap::new();
        if let Ok(entries) = std::fs::read_dir(self.pack_dir()) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                snapshot.insert(name, std::fs::read(entry.path()).unwrap());
            }
        }
        snapshot
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// A worktree with enough files across enough directories that the parallel walk and tree build
/// actually fan out (rather than falling under a serial threshold).
fn populate(warehouse: &Warehouse) {
    for dir in 0..8 {
        for file in 0..8 {
            warehouse.write_file(
                &format!("dir{dir}/sub{}/file{file}.txt", file % 3),
                &format!("content of dir {dir} file {file}\nshared boilerplate line one\nshared boilerplate line two\n"),
            );
        }
    }
}

#[test]
fn identical_worktrees_produce_identical_tree_hashes() {
    // Two independent processes, two independent worker pools, the same bytes on disk: the tree
    // hash is a pure function of content, so a race in the parallel tree build would show up here
    // as a mismatch. Repeated because a scheduling-dependent bug would be intermittent.
    let mut hashes = Vec::new();
    for run in 0..4 {
        let warehouse = Warehouse::new(&format!("tree-{run}"));
        warehouse.prepare();
        populate(&warehouse);
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "layout"]);
        hashes.push(warehouse.head_tree_hash());
    }
    assert!(hashes.iter().all(|h| *h == hashes[0]),
        "identical worktrees produced different tree hashes across runs: {hashes:?}");
}

#[test]
fn a_scoped_stack_is_byte_reproducible_and_matches_a_full_stack() {
    // The scoped overlay (§7.6) reuses the parallel per-directory tree build for its in-scope
    // subtree, then splices the out-of-scope siblings back by hash. Two properties must hold under
    // the worker pool's scheduling: independent runs reassemble the *same* content address, and it
    // is byte-identical to a full workspace stacking the same change (the stage-1 invariant).
    let mut hashes = Vec::new();

    for run in 0..3 {
        let warehouse = Warehouse::new(&format!("scoped-{run}"));
        warehouse.prepare();
        populate(&warehouse);
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "base"]);

        let siblings = warehouse.root.parent().unwrap();
        let full_dir = siblings.join("full-bay");
        let scoped_dir = siblings.join("scoped-bay");
        let _ = std::fs::remove_dir_all(&full_dir);
        let _ = std::fs::remove_dir_all(&scoped_dir);

        warehouse.run_ok(&["bay", "add", "full", full_dir.to_str().unwrap()]);
        warehouse.run_ok(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "dir0/sub0"]);

        // The same in-scope edit across several files in both bays, so the parallel build fans out.
        for file in 0..8 {
            let relative = format!("dir0/sub0/file{file}.txt");
            let content = format!("scoped edit of file {file}\nshared boilerplate line one\n");
            if full_dir.join(&relative).exists() {
                std::fs::write(full_dir.join(&relative), &content).unwrap();
                std::fs::write(scoped_dir.join(&relative), &content).unwrap();
            }
        }

        warehouse.run_ok_at(&full_dir, &["load", "."]);
        warehouse.run_ok_at(&full_dir, &["stack", "edit in full"]);
        warehouse.run_ok_at(&scoped_dir, &["load", "."]);
        warehouse.run_ok_at(&scoped_dir, &["stack", "edit in scoped"]);

        let full_tree = warehouse.parcel_tree_hash("full");
        let scoped_tree = warehouse.parcel_tree_hash("scoped");
        assert_eq!(full_tree, scoped_tree,
            "a scoped stack must produce a byte-identical root tree to a full stack");

        hashes.push(scoped_tree);

        let _ = std::fs::remove_dir_all(&full_dir);
        let _ = std::fs::remove_dir_all(&scoped_dir);
    }

    assert!(hashes.iter().all(|hash| *hash == hashes[0]),
        "a scoped stack must be byte-reproducible across independent runs: {hashes:?}");
}

#[test]
fn stocktake_output_is_stable_across_runs() {
    // The staging walk fans out per directory; its change list is sorted before display, so the
    // output must be identical every time regardless of which worker finished first.
    let warehouse = Warehouse::new("stocktake");
    warehouse.prepare();
    populate(&warehouse);
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    // A mix of staged and unstaged changes so the walk has real work to order.
    for dir in 0..8 {
        warehouse.write_file(&format!("dir{dir}/sub0/file0.txt"), &format!("edited {dir}\n"));
    }
    warehouse.write_file("newfile.txt", "brand new\n");
    warehouse.run_ok(&["load", "dir0"]);

    let first = warehouse.run_ok(&["stocktake"]).stdout;
    for _ in 0..6 {
        assert_eq!(warehouse.run_ok(&["stocktake"]).stdout, first,
            "stocktake output must be identical across repeated runs");
    }
}

#[test]
fn repacking_is_byte_reproducible() {
    // The pack id is derived from the pack's byte layout and repacks tie-break deterministically,
    // so consolidating an already-packed store must reproduce the *same* pack files — not rewrite
    // them under new names every run (the churn the layout-derived id fixed).
    let warehouse = Warehouse::new("repack");
    warehouse.prepare();

    // Several revisions of similar files, so the store holds many objects and real deltas.
    for revision in 0..5 {
        for file in 0..6 {
            warehouse.write_file(
                &format!("mod{file}.txt"),
                &format!("file {file} revision {revision}\nshared line a\nshared line b\nshared line c\n"),
            );
        }
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", &format!("revision {revision}")]);
    }

    // Pack the loose objects, then consolidate into a single steady-state pack set.
    warehouse.run_ok(&["compact"]);
    warehouse.run_ok(&["compact", "--all"]);
    let first = warehouse.pack_snapshot();
    assert!(!first.is_empty(), "compaction should have produced pack files");

    // A second consolidation of the same object set must land byte-for-byte on the same files.
    warehouse.run_ok(&["compact", "--all"]);
    let second = warehouse.pack_snapshot();

    assert_eq!(second.keys().collect::<Vec<_>>(), first.keys().collect::<Vec<_>>(),
        "a steady-state repack must reuse the same pack filenames, not churn them");
    assert_eq!(second, first, "a steady-state repack must reproduce byte-identical pack files");
}

#[test]
fn a_mutating_command_is_refused_while_the_warehouse_lock_is_held() {
    // The warehouse lock is an on-disk sentinel, so another process's held lock is exactly a lock
    // file on disk. A mutating command must be refused (with the exit code and message that name
    // the lock), while a read-only command is unaffected.
    let warehouse = Warehouse::new("lock");
    warehouse.prepare();
    warehouse.write_file("a.txt", "hello\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    // Simulate the other process holding the lock.
    let lock = warehouse.root.join(".forklift").join("lock");
    std::fs::write(&lock, "424242\n").unwrap();

    warehouse.write_file("a.txt", "changed\n");
    let refused = warehouse.run(&["load", "a.txt"]);
    assert!(!refused.status.success(), "a mutating command must be refused while the lock is held");
    assert_eq!(refused.status.code(), Some(6), "the warehouse-locked exit code is 6");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("locked by another forklift process"),
        "the refusal must name the lock: {}", String::from_utf8_lossy(&refused.stderr));

    // A read-only command does not take the lock, so it still works.
    let stocktake = warehouse.run(&["stocktake"]);
    assert!(stocktake.status.success(), "a read-only command must not be blocked by the lock");

    // Once the lock is cleared (the other process finished / the operator removed a stale lock),
    // the mutating command works again.
    std::fs::remove_file(&lock).unwrap();
    warehouse.run_ok(&["load", "a.txt"]);
    warehouse.run_ok(&["stack", "after the lock cleared"]);
}
