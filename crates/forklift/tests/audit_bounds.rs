//! The incremental audit reads nothing behind the head it already trusts.
//!
//! These tests do not count reads — they make the reads *impossible*. A real warehouse is
//! built with the CLI, the commit-graph is warmed, and then the parcel objects behind the
//! already-verified head are **deleted from the object store**. A bounded audit still
//! succeeds, which proves it never touched them; the full `audit` over the same warehouse
//! fails, which proves the deletion was real and the test could have noticed.
//!
//! Two shapes matter, and only the second one ever went wrong:
//!
//! * a **linear** lift, whose frontier is the single hash `old_head`; and
//! * a **merge** lift whose second parent forks *below* `old_head`. Its frontier is the
//!   merge-base set, which one hash cannot express — so the old walk sailed past the fork
//!   point and re-verified ancestry that was audited when `old_head` was committed.

use std::path::PathBuf;
use std::process::{Command, Output};

use forklift_core::globals::StorageRootScope;
use forklift_core::util::{
    audit_utils, file_utils, graph_utils, object_utils, office_utils, pallet_utils,
};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// The chunk threshold (bytes): content at or above this is stored chunked. Mirrors
/// `chunk_utils::CHUNK_THRESHOLD_BYTES` (a frozen format constant).
const CHUNK_THRESHOLD: usize = 8 * 1024 * 1024;

/// One isolated, signed warehouse with its own home for global config + keys.
struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base =
            std::env::temp_dir().join(format!("forklift-audit-bounds-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.name", "audit@forklift"]);
        warehouse.run_ok(&["config", "operator.identifier", "audit@forklift"]);
        warehouse.run_ok(&["office", "enroll"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    /// Write a file and stack it as a signed parcel; return the new head of whichever pallet
    /// is currently checked out (the merge test stacks on a branch, not on `main`).
    fn stack(&self, file: &str, content: &str, message: &str) -> String {
        self.stack_files(&[(file, content)], message)
    }

    /// Write several files and stack them as **one** parcel; return the new head.
    ///
    /// The one-file [`stack`] cannot express a parcel that introduces a whole directory at once,
    /// and the difference decides whether a fixture discriminates. Splitting `P` — which
    /// introduces `b/` containing two files — into two parcels puts the first file in a tree no
    /// later prune ever explains, so the missing blob is caught under any ordering and the test
    /// passes before and after the fix it exists to pin.
    fn stack_files(&self, files: &[(&str, &str)], message: &str) -> String {
        for (file, content) in files {
            let path = self.root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", message]);

        self.head(&self.current_pallet())
    }

    /// Write a large (chunk-threshold-crossing) file of deterministic bytes and stack it as a
    /// signed parcel; return the new head. The bytes are seeded and RNG-free, so the file chunks
    /// reproducibly.
    fn stack_large(&self, file: &str, seed: u64, size: usize, message: &str) -> String {
        let path = self.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mut bytes = Vec::with_capacity(size);
        let mut state = seed;
        while bytes.len() < size {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            bytes.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        bytes.truncate(size);
        std::fs::write(path, bytes).unwrap();

        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", message]);
        self.head(&self.current_pallet())
    }

    /// Delete an arbitrary object (not a parcel — no signature sidecar) from the store.
    fn delete_object(&self, hash: &str) {
        let objects = self.root.join(".forklift").join("objects").join(&hash[0..2]);
        std::fs::remove_file(objects.join(&hash[2..])).expect("the object existed");
    }

    /// Overwrite an object's on-disk bytes with garbage: the file stays *present* (a presence
    /// check still passes) but its bytes no longer decode/hash to the name, so any read fails.
    fn corrupt_object(&self, hash: &str) {
        let objects = self.root.join(".forklift").join("objects").join(&hash[0..2]);
        std::fs::write(objects.join(&hash[2..]), b"corrupted-not-valid-zstd").expect("the object existed");
    }

    fn current_pallet(&self) -> String {
        std::fs::read_to_string(self.root.join(".forklift").join("pallet"))
            .unwrap()
            .trim()
            .to_string()
    }

    fn head(&self, pallet: &str) -> String {
        let name = pallet.strip_prefix('@').unwrap_or(pallet);
        let dir = if pallet.starts_with('@') { "meta" } else { "pallets" };

        std::fs::read_to_string(self.root.join(".forklift").join(dir).join(name))
            .unwrap()
            .trim()
            .to_string()
    }

    /// Delete a parcel's object (and signature sidecar) from the store. The commit-graph
    /// keeps its record, so ancestry is still *navigable* — but nothing can read the parcel.
    fn delete_parcel(&self, hash: &str) {
        let objects = self.root.join(".forklift").join("objects").join(&hash[0..2]);

        std::fs::remove_file(objects.join(&hash[2..])).expect("the parcel object existed");
        let _ = std::fs::remove_file(objects.join(format!("{}.sig", &hash[2..])));
    }

    /// Run `work` inside this warehouse's storage scope.
    fn scoped<T>(&self, work: impl FnOnce() -> T) -> T {
        let _scope = StorageRootScope::enter(&self.root);

        work()
    }
}

/// The trust anchor and verified office state, for the signature audit. Call inside a scope.
fn office() -> (office_utils::TrustAnchor, office_utils::OfficeState) {
    let anchor = office_utils::read_trust_anchor().unwrap().expect("trust is established");
    let office_head = pallet_utils::all_pallet_refs()
        .unwrap()
        .into_iter()
        .find(|(pallet_ref, _)| pallet_ref.to_wire() == "@office")
        .map(|(_, head)| head)
        .expect("an office head");

    let state = audit_utils::verify_office_chain(&anchor, &office_head).expect("the office chain");

    (anchor, state)
}

/// A linear lift audits only its new parcels: the ancestry behind `old_head` is not read,
/// so deleting it changes nothing.
#[test]
fn a_linear_lift_reads_nothing_behind_the_verified_head() {
    let warehouse = Warehouse::new("linear");

    let first = warehouse.stack("app.txt", "v1\n", "first");
    warehouse.stack("app.txt", "v2\n", "second");
    let old_head = warehouse.stack("app.txt", "v3\n", "third");
    let new_head = warehouse.stack("app.txt", "v4\n", "the new segment");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&new_head)).expect("warm the commit-graph");
    });

    // Behind the verified head, and therefore none of the audit's business.
    warehouse.delete_parcel(&first);

    warehouse.scoped(|| {
        let (anchor, state) = office();

        audit_utils::verify_parcel_closure(&new_head, Some(&old_head))
            .expect("the bounded closure check never reads behind the verified head");

        audit_utils::verify_pallet_history(&new_head, &anchor, &state, Some(&old_head))
            .expect("the bounded signature audit never reads behind the verified head");

        // The control: the deletion was real, and an unbounded audit still catches it.
        audit_utils::verify_parcel_closure(&new_head, None)
            .expect_err("a full audit must still find the missing parcel");
    });
}

/// The case that was actually broken. A merge whose second parent forks *below* `old_head`
/// must not walk past the fork point: everything there is reachable from `old_head`, and was
/// verified when `old_head` was committed.
#[test]
fn a_merge_lift_reads_nothing_below_the_fork_point() {
    let warehouse = Warehouse::new("merge");

    // The fork base, and one parcel behind it — both ancestors of `old_head`.
    let root = warehouse.stack("app.txt", "root\n", "root");
    let base = warehouse.stack("app.txt", "base\n", "base");

    // A branch forking at `base`, never lifted: its parcels are genuinely new.
    warehouse.run_ok(&["palletize", "feature"]);
    let branch = warehouse.stack("feature.txt", "from the branch\n", "on the branch");

    // main moves on; that head is what the remote already trusts.
    warehouse.run_ok(&["shift", "main"]);
    let old_head = warehouse.stack("app.txt", "moved on\n", "on main");

    warehouse.run_ok(&["consolidate", "feature"]);
    let new_head = warehouse.head("main");

    warehouse.scoped(|| {
        let parents = graph_utils::parents(&new_head).expect("the merge parcel");
        assert_eq!(parents.len(), 2, "consolidate stacked a real merge parcel");

        graph_utils::build_from_heads(std::slice::from_ref(&new_head)).expect("warm the commit-graph");
    });

    // `root` and `base` are below the fork. They are ancestors of `old_head`, so an audit of
    // the merge has no business reading them — but the old single-hash frontier did, because
    // the walk reached them through the branch without ever passing `old_head`.
    warehouse.delete_parcel(&root);
    warehouse.delete_parcel(&base);

    warehouse.scoped(|| {
        let (anchor, state) = office();

        // The new segment is exactly the merge parcel and the branch parcel.
        let fresh = audit_utils::new_parcels(&new_head, Some(&old_head)).expect("the new segment");
        assert_eq!(fresh, vec![new_head.clone(), branch.clone()]);

        audit_utils::verify_parcel_closure(&new_head, Some(&old_head))
            .expect("the bounded closure check stops at the merge base");

        audit_utils::verify_pallet_history(&new_head, &anchor, &state, Some(&old_head))
            .expect("the bounded signature audit stops at the merge base");

        // The control.
        audit_utils::verify_parcel_closure(&new_head, None)
            .expect_err("a full audit must still find the missing parcels");
    });
}

/// The commit-gate closure audit descends a chunked file's recipe and presence-checks every
/// chunk **non-tolerantly** (§9.4b W4): a ref must never advance over a chunked file whose chunks
/// never reached the store, or the file is silently unmaterializable forever. Deleting one chunk —
/// while the recipe itself stays present — makes the closure check fail, exactly the failure mode a
/// walk that stopped at the recipe hash would have missed.
#[test]
fn the_closure_check_fails_when_a_chunk_of_a_chunked_file_is_missing() {
    let warehouse = Warehouse::new("chunk-missing");

    warehouse.stack("small.txt", "a small file\n", "first");
    let head = warehouse.stack_large("big.bin", 0xABCD, CHUNK_THRESHOLD + 50_000, "a giant");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&head)).expect("warm the commit-graph");

        // The whole closure — recipe and every chunk — is present, so the check passes.
        audit_utils::verify_parcel_closure(&head, None).expect("all chunks present");

        // Resolve the chunked file's recipe and pick one of its chunks to delete.
        let tree = object_utils::load_parcel(&head).expect("the head parcel").tree_hash;
        let (recipe_hash, item_type) = object_utils::resolve_tree_file(&tree, "big.bin")
            .expect("resolve")
            .expect("big.bin is tracked");
        assert!(item_type.is_chunked(), "the giant is stored chunked");

        let recipe = object_utils::load_recipe(&recipe_hash).expect("the recipe");
        let victim = recipe.chunks[0].hash.clone();

        // The recipe stays; only a chunk is gone — a walk stopping at the recipe hash would pass.
        warehouse.delete_object(&victim);
        assert!(
            file_utils::does_object_exist(&recipe_hash).unwrap(),
            "the recipe itself is still present"
        );

        let err = audit_utils::verify_parcel_closure(&head, None)
            .expect_err("a missing chunk must fail the closure check");
        assert!(
            err.contains(&victim) && err.contains("missing"),
            "the error names the missing chunk: {}",
            err
        );
    });
}

/// The subtree prune (§9.4b W1): a push that leaves a large chunked file untouched must not
/// re-presence-check its ~million chunks. Proven by making the check *impossible* — a chunk of the
/// unchanged file is deleted, yet the incremental check still passes, because the file's subtree is
/// byte-identical to the prior head's and is skipped whole. The control (a full audit, and a later
/// push that *does* touch the file) proves the deletion was real and that W4 still bites the moment
/// the file changes.
#[test]
fn the_prune_skips_an_unchanged_chunked_file_but_a_touching_push_still_catches_a_missing_chunk() {
    let warehouse = Warehouse::new("prune-unchanged-chunked");

    warehouse.stack("small.txt", "v1\n", "first");
    // Introduce the large chunked file under data/.
    let with_big =
        warehouse.stack_large("data/big.bin", 0x1234, CHUNK_THRESHOLD + 40_000, "add the giant");
    // A push that touches ONLY small.txt — data/big.bin is byte-identical across this step.
    let touched_small = warehouse.stack("small.txt", "v2\n", "touch only the small file");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&touched_small)).expect("warm the graph");

        // Everything is present, so the incremental check passes to begin with.
        audit_utils::verify_parcel_closure(&touched_small, Some(&with_big)).expect("all present");

        // Delete a chunk of the (unchanged) big.bin — the recipe itself stays present.
        let big_tree = object_utils::load_parcel(&with_big).unwrap().tree_hash;
        let (recipe_hash, item_type) = object_utils::resolve_tree_file(&big_tree, "data/big.bin")
            .unwrap()
            .expect("big.bin is tracked");
        assert!(item_type.is_chunked(), "the giant is stored chunked");
        let victim = object_utils::load_recipe(&recipe_hash).unwrap().chunks[0].hash.clone();
        warehouse.delete_object(&victim);

        // THE PRUNE: `with_big` → `touched_small` did not change data/big.bin, so its subtree is
        // pruned by pure hash comparison — the missing chunk is never even looked for. (The same
        // trust the incremental audit already extends to everything behind `known_complete`.)
        audit_utils::verify_parcel_closure(&touched_small, Some(&with_big))
            .expect("an unchanged chunked file's chunks are not re-checked (the W1 prune)");

        // The control: a FULL audit walks big.bin and catches the hole — so the incremental pass
        // skipped it by the prune, not because the deletion failed to take.
        let err = audit_utils::verify_parcel_closure(&touched_small, None)
            .expect_err("a full audit walks every chunk and finds the hole");
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });

    // A later push that DOES touch data/big.bin must catch a missing chunk in the changed file —
    // the prune never weakens W4 for content the push actually introduced.
    let changed_big =
        warehouse.stack_large("data/big.bin", 0x9999, CHUNK_THRESHOLD + 40_000, "rewrite the giant");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&changed_big)).expect("warm the graph");

        let big_tree = object_utils::load_parcel(&changed_big).unwrap().tree_hash;
        let (recipe_hash, _) = object_utils::resolve_tree_file(&big_tree, "data/big.bin")
            .unwrap()
            .expect("big.bin is tracked");
        let victim = object_utils::load_recipe(&recipe_hash).unwrap().chunks[0].hash.clone();
        warehouse.delete_object(&victim);

        // data/big.bin CHANGED across `touched_small` → `changed_big`, so its subtree is walked in
        // full and the missing chunk fails the non-tolerant W4 descent.
        let err = audit_utils::verify_parcel_closure(&changed_big, Some(&touched_small))
            .expect_err("a changed chunked file with a missing chunk must fail (W4 preserved)");
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });
}

/// `audit --full` re-reads every present chunk's bytes (§9.4b): a chunk whose on-disk bytes are
/// corrupted — but still *present* — passes a normal (presence-only) audit yet fails a `--full`
/// audit, because the content-addressed re-read re-hashes it and finds the mismatch. This is the
/// integrity a normal audit deliberately does not pay for, made explicit.
/// FORK-94, the soundness half: a child must never shield its own parent's audit.
///
/// The prune's candidate bases now include the parcel's **immediate parents**, not only
/// `known_complete`. A parent inside the same push vouches for content only once its own
/// top-level audit has run — so `verify_parcel_closure_with` walks the segment parents-first,
/// and `verify_tree_closure` marks a hash visited before checking the bases.
///
/// Those two are sound together and unsound apart. `new_parcels` returns the segment
/// breadth-first *from head*, so without the sort a child is audited first:
///
/// ```text
///   K → P (b/ = {y, g1})  →  N1 (b/ = {y, g2})  →  N2 (b/ = {y, g1})
/// ```
///
/// `N2` is audited first, prunes `y` against pending `N1`, completes its descent of `b/` and
/// marks that tree; `N1` likewise defers `y` to pending `P`; and `P`'s own audit — the one that
/// was to discharge the chain — reaches an already-visited tree and returns. `y`'s blob is never
/// presence-checked and the ref advances over content this push introduced. Two independent
/// adversarial passes constructed this before it was implemented; today's single-base code
/// refuses the same push, so the composed remedy would have regressed below what it replaced.
#[test]
fn a_child_cannot_shield_the_parent_that_introduced_a_missing_blob() {
    let warehouse = Warehouse::new("fork94-child-shields-parent");

    let k = warehouse.stack("base.txt", "base\n", "K");

    // P introduces b/ whole — both files in one parcel, which is what makes this discriminate.
    let p = warehouse.stack_files(&[("b/y.txt", "y\n"), ("b/g.txt", "g1\n")], "P adds b/");
    // N1 changes only g, so b/ differs from P's; N2 puts it back, so b/ equals P's again.
    warehouse.stack("b/g.txt", "g2\n", "N1 edits g");
    let n2 = warehouse.stack("b/g.txt", "g1\n", "N2 reverts g");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&n2)).expect("warm the graph");
        audit_utils::verify_parcel_closure(&n2, Some(&k)).expect("everything is present to begin");

        // y.txt is introduced by P and never changes after. Its blob is what a wrong order
        // lets slip: explained by a parent whose own audit is still pending.
        let p_tree = object_utils::load_parcel(&p).unwrap().tree_hash;
        let (victim, _) = object_utils::resolve_tree_file(&p_tree, "b/y.txt")
            .unwrap()
            .expect("y.txt is tracked");
        warehouse.delete_object(&victim);

        let err = audit_utils::verify_parcel_closure(&n2, Some(&k))
            .expect_err("the parcel that introduced y.txt must still presence-check its blob");
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });
}

/// FORK-94, the cost half: the candidate set is the **union** of `known_complete`'s subtree and
/// the parcel's parents', never the parents alone.
///
/// Replacing rather than unioning is not never-worse, and a plain revert is the counterexample
/// that killed the first form of this fix. Content-addressing lets content move away and come
/// back: `K` holds a chunked file, `P` rewrites it, `N` puts `K`'s version back. `N`'s subtree is
/// byte-identical to `known_complete`'s, so today — and under the union — it is pruned in one
/// hash comparison. Under a parents-only prune it matches no parent, and the whole subtree is
/// descended: the revert pays the diff cost twice where the current design pays it once.
///
/// The missing chunk is how that difference becomes observable: the union never looks for it
/// (`K` already vouched for that closure — the widened skip set the memo states as this fix's
/// price), a parents-only prune descends and fails on it.
#[test]
fn a_revert_to_the_known_complete_content_is_still_pruned_whole() {
    let warehouse = Warehouse::new("fork94-revert-union");

    warehouse.stack("base.txt", "base\n", "base");
    let k = warehouse.stack_large("w/big.bin", 0x1111, CHUNK_THRESHOLD + 40_000, "K's giant");
    warehouse.stack_large("w/big.bin", 0x2222, CHUNK_THRESHOLD + 40_000, "P rewrites it");
    // Same seed and size as K, so this reverts w/ to byte-identical content — and, by
    // content-addressing, to the identical subtree hash.
    let n = warehouse.stack_large("w/big.bin", 0x1111, CHUNK_THRESHOLD + 40_000, "N reverts it");

    warehouse.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&n)).expect("warm the graph");

        let k_tree = object_utils::load_parcel(&k).unwrap().tree_hash;
        let n_tree = object_utils::load_parcel(&n).unwrap().tree_hash;
        assert_eq!(
            object_utils::resolve_tree_file(&k_tree, "w/big.bin").unwrap(),
            object_utils::resolve_tree_file(&n_tree, "w/big.bin").unwrap(),
            "the revert must reproduce K's content exactly, or this fixture proves nothing"
        );

        let (recipe_hash, item_type) = object_utils::resolve_tree_file(&n_tree, "w/big.bin")
            .unwrap()
            .expect("big.bin is tracked");
        assert!(item_type.is_chunked(), "the giant is stored chunked");
        let victim = object_utils::load_recipe(&recipe_hash).unwrap().chunks[0].hash.clone();
        warehouse.delete_object(&victim);

        audit_utils::verify_parcel_closure(&n, Some(&k))
            .expect("a subtree equal to known_complete's is pruned whole, revert or not");

        // The control: a full audit does descend it, so the deletion was real and this fixture
        // could have noticed.
        let err = audit_utils::verify_parcel_closure(&n, None)
            .expect_err("a full audit walks every chunk and finds the hole");
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });
}

/// FORK-94, the second reachable class: the same subtree hash presented at a **relocated** path.
///
/// Both prunes are path-wise — a subtree is explained only by a base's subtree at the same name —
/// so a hash recurring where no base carries that name is explained by nothing, however trusted
/// its content. The client has always closed this by recording a hash in its visited set *before*
/// the base check, and its comment names the case; the server pruned before marking, so a pruned
/// hash was never marked and its recurrence elsewhere was descended in full.
///
/// This needs no merge, no sparseness and no staleness — one linear commit that copies an
/// unchanged directory to a second path.
///
/// **The class is closed only when the explained occurrence is walked first, and that is a
/// property of sibling order, not of the fix.** Subtrees are walked by name, so the copy is free
/// when it sorts *after* its source and is descended in full when it sorts before. Both
/// directions are asserted below, because the honest statement of what this fix buys is "the
/// recurrence is free once the hash has been settled", not "relocation is free". A test asserting
/// only the first half would read as the stronger claim.
#[test]
fn a_copied_subtree_is_free_once_its_source_has_been_settled_but_not_before() {
    // Explained first: `app/` sorts before `zz-vendor/`, so the source is pruned (and marked)
    // before the walk reaches the copy.
    let settled_first = Warehouse::new("fork94-relocation-after");
    let k = settled_first.stack_files(
        &[("app/a.txt", "a\n"), ("app/b.txt", "b\n"), ("other.txt", "v1\n")],
        "K",
    );
    let n = settled_first.stack_files(
        &[("other.txt", "v2\n"), ("zz-vendor/a.txt", "a\n"), ("zz-vendor/b.txt", "b\n")],
        "N copies app/ to zz-vendor/",
    );

    settled_first.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&n)).expect("warm the graph");

        let k_tree = object_utils::load_parcel(&k).unwrap().tree_hash;
        let n_tree = object_utils::load_parcel(&n).unwrap().tree_hash;
        assert_eq!(
            object_utils::resolve_tree_file(&n_tree, "zz-vendor/b.txt").unwrap(),
            object_utils::resolve_tree_file(&k_tree, "app/b.txt").unwrap(),
            "the copy must be byte-identical, or the subtree hashes differ and nothing is pruned"
        );

        let (victim, _) = object_utils::resolve_tree_file(&k_tree, "app/b.txt")
            .unwrap()
            .expect("b.txt is tracked");
        settled_first.delete_object(&victim);

        audit_utils::verify_parcel_closure(&n, Some(&k))
            .expect("app/ is settled first, so its hash recurring at zz-vendor/ costs nothing");

        let err = audit_utils::verify_parcel_closure(&n, None)
            .expect_err("a full audit walks both paths and finds the hole");
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });

    // Copy first: `aa-vendor/` sorts before `web/`, so the recurrence is reached before anything
    // has settled that hash, and it is descended in full. This is the residual the memo states.
    let copy_first = Warehouse::new("fork94-relocation-before");
    let k2 = copy_first.stack_files(
        &[("web/a.txt", "a\n"), ("web/b.txt", "b\n"), ("other.txt", "v1\n")],
        "K",
    );
    let n2 = copy_first.stack_files(
        &[("other.txt", "v2\n"), ("aa-vendor/a.txt", "a\n"), ("aa-vendor/b.txt", "b\n")],
        "N copies web/ to aa-vendor/",
    );

    copy_first.scoped(|| {
        graph_utils::build_from_heads(std::slice::from_ref(&n2)).expect("warm the graph");

        let k2_tree = object_utils::load_parcel(&k2).unwrap().tree_hash;
        let (victim, _) = object_utils::resolve_tree_file(&k2_tree, "web/b.txt")
            .unwrap()
            .expect("b.txt is tracked");
        copy_first.delete_object(&victim);

        let err = audit_utils::verify_parcel_closure(&n2, Some(&k2)).expect_err(
            "a copy walked before its source is genuinely new content at that path, and is \
             descended — if this ever starts passing, the class is closed under any order and \
             this test's doc comment is overcautious",
        );
        assert!(err.contains(&victim) && err.contains("missing"), "{}", err);
    });
}

#[test]
fn full_audit_re_reads_chunks_and_catches_corruption_a_presence_check_misses() {
    let warehouse = Warehouse::new("full-audit-chunk");

    let head = warehouse.stack_large("big.bin", 0x5EED, CHUNK_THRESHOLD + 60_000, "a giant");

    warehouse.scoped(|| {
        let full = forklift_core::util::scope_utils::MaterializationScope::full();

        // Corrupt one chunk's bytes but leave the object present. (Corrupt before ever reading the
        // chunk in this process, so the read cache never holds the good bytes — the on-disk
        // corruption is what the re-read must catch. Healthy --full success is proven end to end by
        // `full_audit_cli_reports_the_content_level_it_ran`, in its own fresh process.)
        let tree = object_utils::load_parcel(&head).unwrap().tree_hash;
        let (recipe_hash, _) = object_utils::resolve_tree_file(&tree, "big.bin").unwrap().unwrap();
        let victim = object_utils::load_recipe(&recipe_hash).unwrap().chunks[0].hash.clone();
        warehouse.corrupt_object(&victim);
        assert!(file_utils::does_object_exist(&victim).unwrap(), "the chunk file is still present");

        // Presence-only audit still passes — it never reads the bytes.
        audit_utils::verify_parcel_closure_scoped(&head, None, &full, false)
            .expect("a normal audit presence-checks chunks and does not notice corrupted bytes");

        // --full re-reads the chunk, re-hashes it, and fails.
        audit_utils::verify_parcel_closure_scoped(&head, None, &full, true)
            .expect_err("a --full audit re-reads every chunk and catches the corruption");
    });
}

/// `forklift audit --full` on a healthy chunked warehouse succeeds end to end and reports the
/// stronger content level in its `--json` envelope, so a consumer can tell a `--full` pass from a
/// presence-only one.
#[test]
fn full_audit_cli_reports_the_content_level_it_ran() {
    let warehouse = Warehouse::new("full-audit-cli");
    warehouse.stack_large("data/big.bin", 0xC0FFEE, CHUNK_THRESHOLD + 30_000, "a giant");

    let out = warehouse.run_ok(&["audit", "--full", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(stdout.contains("\"full\":true") || stdout.contains("\"full\": true"),
        "the --json envelope echoes the --full level: {}", stdout);
    assert!(stdout.contains("re-read and re-hashed"),
        "the --json envelope states chunks were re-read under --full: {}", stdout);

    // A plain audit of the same warehouse must NOT claim the full level.
    let plain = warehouse.run_ok(&["audit", "--json"]);
    let plain_stdout = String::from_utf8_lossy(&plain.stdout);
    assert!(plain_stdout.contains("\"full\":false") || plain_stdout.contains("\"full\": false"),
        "a normal audit reports full=false: {}", plain_stdout);
}

/// The office chain is verified once per `(warehouse, anchor, office head)`, not once per
/// ref update — and the memo is keyed by warehouse, so it can never answer for a store that
/// does not hold the chain.
#[test]
fn a_verified_office_chain_is_memoized_per_warehouse() {
    let warehouse = Warehouse::new("office-memo");
    warehouse.stack("app.txt", "v1\n", "first");

    let office_head = warehouse.scoped(|| {
        pallet_utils::all_pallet_refs()
            .unwrap()
            .into_iter()
            .find(|(pallet_ref, _)| pallet_ref.to_wire() == "@office")
            .map(|(_, head)| head)
            .expect("an office head")
    });

    let anchor =
        warehouse.scoped(|| office_utils::read_trust_anchor().unwrap().expect("trust"));

    // First call verifies for real.
    let first = warehouse
        .scoped(|| audit_utils::verify_office_chain_memoized(&anchor, &office_head))
        .expect("the office chain verifies");

    // Make re-verification impossible: the chain's parcels are gone.
    warehouse.delete_parcel(&office_head);

    let memoized = warehouse
        .scoped(|| audit_utils::verify_office_chain_memoized(&anchor, &office_head))
        .expect("the memo answers without touching the chain");
    assert_eq!(memoized.keys.len(), first.keys.len());
    assert_eq!(memoized.users.len(), first.users.len());

    // The uncached path still reads, and still fails.
    assert!(
        warehouse.scoped(|| audit_utils::verify_office_chain(&anchor, &office_head)).is_err(),
        "the deletion was real"
    );

    // The tenant boundary: another warehouse, same anchor and head, must not inherit the
    // verified state — its object store holds no such chain.
    let other = Warehouse::new("office-memo-other");

    assert!(
        other.scoped(|| audit_utils::verify_office_chain_memoized(&anchor, &office_head)).is_err(),
        "a memo must never answer across warehouses"
    );
}

/// The frontier's edge cases, stated directly.
#[test]
fn the_new_segment_is_the_gap_between_two_heads() {
    let warehouse = Warehouse::new("frontier");

    let first = warehouse.stack("app.txt", "v1\n", "first");
    let second = warehouse.stack("app.txt", "v2\n", "second");
    let third = warehouse.stack("app.txt", "v3\n", "third");

    warehouse.scoped(|| {
        // Nothing is new relative to itself.
        assert!(audit_utils::new_parcels(&third, Some(&third)).unwrap().is_empty());

        // The gap, newest first (breadth-first from the head).
        assert_eq!(
            audit_utils::new_parcels(&third, Some(&first)).unwrap(),
            vec![third.clone(), second.clone()]
        );

        // No bound walks the whole history, and the office parcels are not in this pallet.
        let all = audit_utils::new_parcels(&third, None).unwrap();
        assert_eq!(all, vec![third.clone(), second.clone(), first.clone()]);

        // A head behind the bound contributes nothing new.
        assert!(audit_utils::new_parcels(&first, Some(&third)).unwrap().is_empty());
    });
}
