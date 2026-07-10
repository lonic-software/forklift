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
use forklift_core::util::{audit_utils, graph_utils, office_utils, pallet_utils};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

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
        let path = self.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", message]);

        self.head(&self.current_pallet())
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
